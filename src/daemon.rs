//! Daemon: supervises one actor per scratchpad, fans Hyprland events out to
//! them and answers the control socket.
//!
//! Control commands are acknowledged as soon as they're queued, so the
//! keybind's `scratchd toggle x` returns in milliseconds whatever the app does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::{self, ScratchConf};
use crate::hypr::{Hypr, lua_str};
use crate::scratch::{Actor, Msg, NOANIM_TAG, Shared};

const QUEUE: usize = 64;

pub fn socket_path(hypr: &Hypr) -> PathBuf {
    hypr.dir().join(".scratchd.sock")
}

pub struct Options {
    pub config: PathBuf,
    pub socket: PathBuf,
    pub log_dir: PathBuf,
}

pub async fn run(hypr: Hypr, opts: Options) -> Result<()> {
    if UnixStream::connect(&opts.socket).await.is_ok() {
        bail!("already running ({})", opts.socket.display());
    }
    let _ = std::fs::remove_file(&opts.socket);
    let listener = UnixListener::bind(&opts.socket).with_context(|| format!("binding {}", opts.socket.display()))?;
    std::fs::create_dir_all(&opts.log_dir).with_context(|| format!("creating {}", opts.log_dir.display()))?;

    let shared = Arc::new(Shared {
        hypr,
        peers: RwLock::new(HashMap::new()),
        confs: RwLock::new(HashMap::new()),
        log_dir: opts.log_dir.clone(),
    });
    reload(&shared, &opts.config).await;
    tokio::spawn(events(shared.clone()));

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let (exit_tx, mut exit_rx) = mpsc::channel::<()>(1);
    info!("ready on {}", opts.socket.display());
    loop {
        tokio::select! {
            conn = listener.accept() => match conn {
                Ok((stream, _)) => {
                    let (shared, config, exit_tx) = (shared.clone(), opts.config.clone(), exit_tx.clone());
                    tokio::spawn(async move {
                        if let Err(e) = serve(stream, &shared, &config, &exit_tx).await {
                            debug!("control connection: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("accept: {e}"),
            },
            _ = hup.recv() => reload(&shared, &opts.config).await,
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = exit_rx.recv() => break,
        }
    }
    info!("exiting");
    let _ = std::fs::remove_file(&opts.socket);
    Ok(())
}

/// (Re)load the config. A config that won't parse keeps the old scratchpads.
pub async fn reload(shared: &Arc<Shared>, path: &Path) {
    let loaded = match config::load(path) {
        Ok(l) => l,
        Err(e) => {
            error!("config: {e:#}");
            shared.hypr.notify_error(&format!("config not loaded: {e:#}")).await;
            return;
        }
    };
    for err in &loaded.errors {
        warn!("config: {err}");
    }
    if loaded.errors.iter().any(|e| e.contains("skipped")) {
        shared.hypr.notify_error("some scratchpads were skipped, see the log").await;
    }

    let new: HashMap<String, Arc<ScratchConf>> =
        loaded.scratches.into_iter().map(|c| (c.name.clone(), Arc::new(c))).collect();
    let old: Vec<String> = shared.confs.read().unwrap().keys().cloned().collect();
    for name in old.iter().filter(|n| !new.contains_key(*n)) {
        info!("{name}: removed from config");
        shared.confs.write().unwrap().remove(name);
        // dropping the sender closes the queue and stops the actor
        shared.peers.write().unwrap().remove(name);
    }
    for (name, conf) in new {
        let existed = shared.confs.write().unwrap().insert(name.clone(), conf.clone()).is_some();
        if existed {
            let _ = send(shared, &name, Msg::Configure(conf));
        } else {
            supervise(shared.clone(), name);
        }
    }
    apply_rules(shared).await;
}

/// Run a scratchpad's actor, restarting it if it panics. The new actor
/// re-adopts the window by matching, so a crash loses nothing visible.
fn supervise(shared: Arc<Shared>, name: String) {
    tokio::spawn(async move {
        loop {
            let Some(conf) = shared.confs.read().unwrap().get(&name).cloned() else { return };
            let (tx, rx) = mpsc::channel(QUEUE);
            let actor = Actor::new(conf, shared.clone(), tx.downgrade());
            let _ = tx.try_send(Msg::Start);
            shared.peers.write().unwrap().insert(name.clone(), tx);
            match tokio::spawn(actor.run(rx)).await {
                Ok(()) => return,
                Err(e) if e.is_panic() => {
                    error!("{name}: actor crashed, restarting: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(_) => return,
            }
        }
    });
}

/// Rules Hyprland drops on config reload: the no-animation tag used for
/// off-screen jumps, and persistent scratchpad workspaces.
async fn apply_rules(shared: &Shared) {
    let mut stmts = vec![format!(
        "hl.window_rule({{name={}, match={{tag={}}}, no_anim=true}})",
        lua_str(NOANIM_TAG),
        lua_str(NOANIM_TAG)
    )];
    for conf in shared.confs.read().unwrap().values() {
        stmts.push(format!("hl.workspace_rule({{workspace={}, persistent=true}})", lua_str(&conf.workspace())));
    }
    if let Err(e) = shared.hypr.eval(&stmts).await {
        warn!("applying rules: {e:#}");
    }
}

fn send(shared: &Shared, name: &str, msg: Msg) -> Result<()> {
    let peers = shared.peers.read().unwrap();
    let Some(tx) = peers.get(name) else { bail!("no scratchpad named {name:?}") };
    tx.try_send(msg).map_err(|e| match e {
        mpsc::error::TrySendError::Full(_) => anyhow::anyhow!("{name} is busy, try again"),
        mpsc::error::TrySendError::Closed(_) => anyhow::anyhow!("{name} is restarting, try again"),
    })
}

fn broadcast(shared: &Shared, mk: impl Fn() -> Msg) {
    for (name, tx) in shared.peers.read().unwrap().iter() {
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(mk()) {
            warn!("{name}: queue full, dropped an event");
        }
    }
}

async fn events(shared: Arc<Shared>) {
    let mut backoff = Duration::from_millis(200);
    loop {
        match UnixStream::connect(shared.hypr.event_socket()).await {
            Ok(stream) => {
                backoff = Duration::from_millis(200);
                let mut lines = BufReader::new(stream).lines();
                let mut active_class = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    on_event(&shared, &line, &mut active_class);
                }
                warn!("event stream closed, reconnecting");
            }
            Err(e) => warn!("event socket: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

fn on_event(shared: &Arc<Shared>, line: &str, active_class: &mut String) {
    let Some((name, data)) = line.split_once(">>") else { return };
    match name {
        "openwindow" => broadcast(shared, || Msg::Opened),
        "closewindow" => {
            let addr = format!("0x{data}");
            broadcast(shared, || Msg::Closed { addr: addr.clone() });
        }
        // `activewindow>>class,title` arrives right before `activewindowv2>>addr`
        "activewindow" => *active_class = data.split_once(',').map_or(data, |(c, _)| c).to_owned(),
        "activewindowv2" => {
            let addr = if data.is_empty() || data == "," { String::new() } else { format!("0x{data}") };
            let class = active_class.clone();
            broadcast(shared, || Msg::Focus { addr: addr.clone(), class: class.clone() });
        }
        "configreloaded" => {
            let shared = shared.clone();
            tokio::spawn(async move { apply_rules(&shared).await });
        }
        _ => {}
    }
}

async fn serve(mut stream: UnixStream, shared: &Arc<Shared>, config: &Path, exit: &mpsc::Sender<()>) -> Result<()> {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(1), (&mut stream).take(4096).read_to_string(&mut line))
        .await
        .context("client too slow")??;
    let reply = match command(line.trim(), shared, config, exit).await {
        Ok(s) => s,
        Err(e) => format!("error: {e:#}"),
    };
    stream.write_all(reply.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn command(line: &str, shared: &Arc<Shared>, config: &Path, exit: &mpsc::Sender<()>) -> Result<String> {
    let mut words = line.split_whitespace();
    let verb = words.next().unwrap_or("");
    let names: Vec<&str> = words.collect();
    let each = |mk: fn() -> Msg| -> Result<String> {
        if names.is_empty() {
            bail!("usage: {verb} <scratchpad>...");
        }
        let all: Vec<String> = if names == ["*"] {
            shared.peers.read().unwrap().keys().cloned().collect()
        } else {
            names.iter().map(|s| s.to_string()).collect()
        };
        let errors: Vec<String> = all.iter().filter_map(|n| send(shared, n, mk()).err()).map(|e| e.to_string()).collect();
        if errors.is_empty() { Ok("ok".into()) } else { bail!("{}", errors.join("; ")) }
    };
    match verb {
        "toggle" => each(|| Msg::Toggle),
        "show" => each(|| Msg::Show),
        "hide" => each(|| Msg::Hide),
        "ping" => Ok("pong".into()),
        "reload" => {
            reload(shared, config).await;
            Ok("ok".into())
        }
        "exit" => {
            let _ = exit.try_send(());
            Ok("ok".into())
        }
        "status" => {
            let peers: Vec<(String, mpsc::Sender<Msg>)> =
                shared.peers.read().unwrap().iter().map(|(n, t)| (n.clone(), t.clone())).collect();
            let mut out = serde_json::Map::new();
            for (name, tx) in peers {
                let (reply, rx) = oneshot::channel();
                let value = match tx.try_send(Msg::Status(reply)) {
                    Ok(()) => tokio::time::timeout(Duration::from_millis(500), rx)
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_else(|| "busy".into()),
                    Err(_) => "busy".into(),
                };
                out.insert(name, value);
            }
            Ok(serde_json::to_string_pretty(&out)?)
        }
        "" => bail!("empty command"),
        other => bail!("unknown command {other:?}"),
    }
}
