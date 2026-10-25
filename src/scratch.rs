//! One actor per scratchpad.
//!
//! Each scratchpad owns a task and a queue, so a slow or broken app only
//! delays its own commands. Launching never blocks the queue either: the
//! actor records what the user wants and acts when the window shows up, so
//! toggling a launching app again just flips that wish.
//!
//! Visibility is read from Hyprland on every command instead of being
//! tracked, so the actor can't drift from what's on screen.
//!
//! Actors run concurrently, so `excludes` could otherwise race: two keys
//! pressed together would each hide the other. Every press gets a number
//! from a shared counter and an exclusion from an earlier press than the
//! one that showed us is ignored, so the last key pressed wins.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::ScratchConf;
use crate::geometry::{offscreen, parse_pair, resting, slide_out};
use crate::hypr::{Client, Hypr, Monitor, dsp};

/// Minimum delay for Hyprland to apply a batch before the next one.
const ONE_FRAME: Duration = Duration::from_millis(20);
/// Focus changes right after a show are part of the show, not the user leaving.
const AFTER_SHOW_GRACE: Duration = Duration::from_millis(300);
const SPAWN_POLL: Duration = Duration::from_millis(150);
pub const NOANIM_TAG: &str = "scratchd_noanim";

pub enum Msg {
    Toggle { press: u64 },
    Show { press: u64 },
    Hide,
    /// Another scratchpad was shown for `press` and excludes this one.
    Excluded { press: u64 },
    Focus { addr: String, class: String },
    Opened,
    Closed { addr: String },
    SpawnTick { epoch: u64 },
    SpawnExited { epoch: u64, success: bool, status: String },
    UnfocusFire { epoch: u64 },
    Configure(Arc<ScratchConf>),
    Start,
    Status(oneshot::Sender<serde_json::Value>),
}

pub struct Shared {
    pub hypr: Hypr,
    pub peers: RwLock<HashMap<String, mpsc::Sender<Msg>>>,
    pub confs: RwLock<HashMap<String, Arc<ScratchConf>>>,
    pub log_dir: PathBuf,
    /// Numbers key presses, see the module docs.
    pub presses: AtomicU64,
}

struct Spawn {
    epoch: u64,
    started: Instant,
    want_visible: bool,
    /// The press that last asked for it to be shown.
    press: u64,
}

pub struct Actor {
    conf: Arc<ScratchConf>,
    shared: Arc<Shared>,
    me: mpsc::WeakSender<Msg>,
    addr: Option<String>,
    spawned_pid: Option<u32>,
    spawn: Option<Spawn>,
    epoch: u64,
    last_shown: Option<Instant>,
    /// What we last did; only a hint for the unfocus check.
    shown: bool,
    /// The press that last showed us.
    shown_press: u64,
    /// The app's other windows that moved with the last show, and their
    /// offsets from the main window saved at the last hide.
    extras: HashSet<String>,
    extra_offsets: HashMap<String, (i32, i32)>,
    unfocus_gen: u64,
}

enum Present {
    Here,
    Elsewhere,
    Hidden,
}

impl Actor {
    pub fn new(conf: Arc<ScratchConf>, shared: Arc<Shared>, me: mpsc::WeakSender<Msg>) -> Self {
        Self {
            conf,
            shared,
            me,
            addr: None,
            spawned_pid: None,
            spawn: None,
            epoch: 0,
            last_shown: None,
            shown: false,
            shown_press: 0,
            extras: HashSet::new(),
            extra_offsets: HashMap::new(),
            unfocus_gen: 0,
        }
    }

    fn name(&self) -> &str {
        &self.conf.name
    }

    fn hypr(&self) -> &Hypr {
        &self.shared.hypr
    }

    pub async fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = self.handle(msg).await {
                warn!("{}: {e:#}", self.name());
            }
        }
        debug!("{}: actor stopped", self.name());
    }

    async fn handle(&mut self, msg: Msg) -> Result<()> {
        match msg {
            Msg::Toggle { press } => match &mut self.spawn {
                Some(sp) => {
                    sp.want_visible = !sp.want_visible;
                    sp.press = press;
                    info!("{}: still launching, will {} it", self.conf.name, if sp.want_visible { "show" } else { "hide" });
                }
                None => self.toggle(press).await?,
            },
            Msg::Show { press } => match &mut self.spawn {
                Some(sp) => (sp.want_visible, sp.press) = (true, press),
                None => self.show_or_launch(press).await?,
            },
            Msg::Hide => match &mut self.spawn {
                Some(sp) => sp.want_visible = false,
                None => self.hide_if_out().await?,
            },
            Msg::Excluded { press } => match &mut self.spawn {
                Some(sp) if press > sp.press => sp.want_visible = false,
                Some(_) => {}
                None if press <= self.shown_press => {
                    debug!("{}: ignoring exclusion from an earlier press", self.conf.name);
                }
                None => self.hide_if_out().await?,
            },
            Msg::Focus { addr, class } => self.on_focus(&addr, &class),
            Msg::Opened => {
                if self.spawn.is_some() {
                    self.poll_spawn().await?;
                }
            }
            Msg::SpawnTick { epoch } => {
                if self.spawn.as_ref().is_some_and(|s| s.epoch == epoch) {
                    self.poll_spawn().await?;
                }
            }
            Msg::SpawnExited { epoch, success, status } => {
                if self.spawn.as_ref().is_some_and(|s| s.epoch == epoch) {
                    if success {
                        debug!("{}: launcher exited cleanly, still waiting for the window", self.name());
                    } else {
                        self.poll_spawn().await?;
                        if self.spawn.is_some() {
                            self.spawn = None;
                            self.fail(&format!("command exited with {status} before opening a window")).await;
                        }
                    }
                }
            }
            Msg::Closed { addr } => {
                self.extras.remove(&addr);
                self.extra_offsets.remove(&addr);
                if self.addr.as_deref() == Some(addr.as_str()) {
                    debug!("{}: window {addr} closed", self.name());
                    self.addr = None;
                    self.shown = false;
                    self.unfocus_gen += 1;
                }
            }
            Msg::UnfocusFire { epoch } => {
                if epoch == self.unfocus_gen {
                    let active = self.hypr().active_window().await?.address;
                    if self.addr.as_deref() != Some(active.as_str()) && !self.extras.contains(&active) {
                        debug!("{}: hiding, focus moved away", self.name());
                        self.hide_if_out().await?;
                    }
                }
            }
            Msg::Configure(conf) => self.conf = conf,
            Msg::Start => self.start().await?,
            Msg::Status(reply) => {
                let _ = reply.send(json!({
                    "name": self.conf.name,
                    "address": self.addr,
                    "launching": self.spawn.as_ref().map(|s| json!({
                        "secs": s.started.elapsed().as_secs_f64(),
                        "show_when_ready": s.want_visible,
                    })),
                    "shown": self.shown,
                    "extras": self.extras,
                }));
            }
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<(Vec<Client>, Vec<Monitor>)> {
        tokio::try_join!(self.hypr().clients(), self.hypr().monitors())
    }

    /// Our window: the remembered address if it's still ours, else the first
    /// matching window, which is adopted.
    fn find(&mut self, clients: &[Client]) -> Option<Client> {
        let matcher = &self.conf.matcher;
        if let Some(addr) = &self.addr {
            match clients.iter().find(|c| &c.address == addr) {
                Some(c) if !matcher.is_stable() || matcher.matches(c, self.spawned_pid) => return Some(c.clone()),
                _ => self.addr = None,
            }
        }
        let c = clients.iter().find(|c| c.mapped && matcher.matches(c, self.spawned_pid))?;
        info!("{}: using window {} ({})", self.conf.name, c.address, c.class);
        self.addr = Some(c.address.clone());
        Some(c.clone())
    }

    /// The app's other windows: same class (or same match when no class is
    /// configured), minus anything parked in another scratchpad.
    fn extra_windows<'a>(&self, clients: &'a [Client], main: &Client) -> Vec<&'a Client> {
        if !self.conf.multi {
            return vec![];
        }
        let own = self.conf.workspace();
        clients
            .iter()
            .filter(|c| c.address != main.address && c.mapped)
            .filter(|c| !c.workspace.name.starts_with("special:S-") || c.workspace.name == own)
            .filter(|c| match &self.conf.class {
                Some(p) => p.matches(&c.class),
                None => self.conf.matcher.matches(c, self.spawned_pid),
            })
            .collect()
    }

    fn presence(&self, c: &Client, mon: &Monitor) -> Present {
        let ws = c.workspace.name.as_str();
        if ws == mon.active_workspace.name
            || (ws == mon.special_workspace.name && !ws.starts_with("special:S-"))
        {
            Present::Here
        } else if ws == self.conf.workspace() {
            Present::Hidden
        } else {
            Present::Elsewhere
        }
    }

    async fn toggle(&mut self, press: u64) -> Result<()> {
        let (clients, monitors) = self.snapshot().await?;
        let mon = focused(&monitors)?;
        match self.find(&clients) {
            None => self.launch(true, press).await,
            Some(c) => match self.presence(&c, mon) {
                Present::Here => self.hide(&c, &clients, &monitors).await,
                Present::Elsewhere | Present::Hidden => self.show(&c, &clients, mon, press).await,
            },
        }
    }

    async fn show_or_launch(&mut self, press: u64) -> Result<()> {
        let (clients, monitors) = self.snapshot().await?;
        let mon = focused(&monitors)?;
        match self.find(&clients) {
            None => self.launch(true, press).await,
            Some(c) => self.show(&c, &clients, mon, press).await,
        }
    }

    async fn hide_if_out(&mut self) -> Result<()> {
        let (clients, monitors) = self.snapshot().await?;
        if let Some(c) = self.find(&clients)
            && c.workspace.name != self.conf.workspace()
        {
            self.hide(&c, &clients, &monitors).await?;
        }
        self.shown = false;
        Ok(())
    }

    /// Adopt a running window, or pre-launch when not lazy.
    async fn start(&mut self) -> Result<()> {
        let clients = self.hypr().clients().await?;
        match self.find(&clients) {
            Some(c) => self.shown = c.workspace.name != self.conf.workspace(),
            None if !self.conf.lazy => self.launch(false, 0).await?,
            None => {}
        }
        Ok(())
    }

    fn exclude_others(&self, press: u64) {
        let peers = self.shared.peers.read().unwrap();
        for (name, tx) in peers.iter().filter(|(n, _)| self.conf.excludes(n)) {
            if tx.try_send(Msg::Excluded { press }).is_err() {
                warn!("{}: couldn't ask {name} to hide (queue full)", self.conf.name);
            }
        }
    }

    async fn show(&mut self, c: &Client, clients: &[Client], mon: &Monitor, press: u64) -> Result<()> {
        info!("{}: show", self.name());
        self.unfocus_gen += 1;
        self.shown_press = press;
        self.exclude_others(press);
        let conf = self.conf.clone();
        let addr = c.address.as_str();
        let hypr = self.hypr().clone();

        let (mut w, mut h) = parse_pair(&conf.size, mon).context("size")?;
        if let Some(max) = &conf.max_size {
            let (mw, mh) = parse_pair(max, mon).context("max_size")?;
            (w, h) = (w.min(mw), h.min(mh));
        }
        // Over a special workspace the user opened (not another scratchpad's),
        // show it there.
        let target = if mon.special_workspace.name.is_empty() || mon.special_workspace.name.starts_with("special:S-") {
            &mon.active_workspace.name
        } else {
            &mon.special_workspace.name
        };

        let mut stage = vec![];
        if !c.floating {
            stage.push(dsp::toggle_float(addr));
        }
        stage.push(dsp::resize(addr, w, h));
        if let Some(anim) = conf.animation {
            // jump off-screen without animating, so the slide in starts there
            let (x, y) = offscreen(anim, mon, (w, h), conf.margin);
            stage.push(dsp::tag(addr, &format!("+{NOANIM_TAG}")));
            stage.push(dsp::move_to(addr, x, y));
        }
        hypr.eval(&stage).await?;

        let mut stage = vec![];
        if conf.animation.is_some() {
            tokio::time::sleep(ONE_FRAME).await;
            stage.push(dsp::tag(addr, &format!("-{NOANIM_TAG}")));
        }
        if c.workspace.name.starts_with("special:") {
            stage.push(dsp::workspace_to_monitor(&c.workspace.name, &mon.name));
        }
        stage.push(dsp::to_workspace(addr, target));
        stage.push(dsp::raise(addr));
        hypr.eval(&stage).await?;

        let (x, y) = match &conf.position {
            Some(p) => {
                let (x, y) = parse_pair(p, mon).context("position")?;
                (x + mon.x, y + mon.y)
            }
            None => resting(conf.animation, mon, (w, h), conf.margin),
        };
        let mut stage = vec![dsp::move_to(addr, x, y)];
        // read the pin state instead of blindly toggling, so it can't drift
        if conf.pinned != c.pinned {
            stage.push(dsp::toggle_pin(addr));
        }
        // bring back the windows that hid with it, where they were relative to it
        let own = conf.workspace();
        let extras: Vec<Client> =
            self.extra_windows(clients, c).into_iter().filter(|e| e.workspace.name == own).cloned().collect();
        self.extras = extras.iter().map(|e| e.address.clone()).collect();
        for e in &extras {
            let (ex, ey) = match self.extra_offsets.get(&e.address) {
                Some((dx, dy)) => (x + dx, y + dy),
                None => resting(None, mon, (e.size[0], e.size[1]), 0),
            };
            stage.push(dsp::to_workspace(&e.address, target));
            stage.push(dsp::move_to(&e.address, ex, ey));
            if conf.pinned != e.pinned {
                stage.push(dsp::toggle_pin(&e.address));
            }
        }
        stage.extend(extras.iter().map(|e| dsp::raise(&e.address)));
        stage.push(dsp::focus(addr));
        hypr.eval(&stage).await?;
        self.last_shown = Some(Instant::now());
        self.shown = true;

        if conf.position.is_some() {
            // apps that resize themselves on map shift the window; put it back
            tokio::time::sleep(ONE_FRAME).await;
            hypr.eval(&[dsp::move_to(addr, x, y)]).await?;
        }
        Ok(())
    }

    async fn hide(&mut self, c: &Client, clients: &[Client], monitors: &[Monitor]) -> Result<()> {
        info!("{}: hide", self.name());
        self.unfocus_gen += 1;
        self.shown = false;
        let conf = self.conf.clone();
        let addr = c.address.as_str();
        let hypr = self.hypr().clone();
        let mon = monitors.iter().find(|m| m.id == c.monitor).map_or_else(|| focused(monitors), Ok)?;
        let size = (c.size[0], c.size[1]);

        // the app's other windows next to it (a call, a dialog) go first, so
        // they don't linger while the main window slides out
        let extras: Vec<&Client> =
            self.extra_windows(clients, c).into_iter().filter(|e| e.workspace.name == c.workspace.name).collect();
        if !extras.is_empty() {
            let mut stage = vec![];
            for e in &extras {
                self.extra_offsets.insert(e.address.clone(), (e.at[0] - c.at[0], e.at[1] - c.at[1]));
                if e.pinned {
                    stage.push(dsp::toggle_pin(&e.address));
                }
                stage.push(dsp::to_workspace(&e.address, &conf.workspace()));
            }
            hypr.eval(&stage).await?;
        }

        if let Some(anim) = conf.animation {
            let (x, y) = slide_out(anim, (c.at[0], c.at[1]), size, conf.margin);
            hypr.eval(&[dsp::move_to(addr, x, y)]).await?;
            tokio::time::sleep(conf.hide_delay).await;
            let (x, y) = offscreen(anim, mon, size, conf.margin);
            hypr.eval(&[dsp::tag(addr, &format!("+{NOANIM_TAG}")), dsp::move_to(addr, x, y)]).await?;
        }
        let mut stage = vec![];
        if c.pinned {
            // a pinned window rides along to every workspace instead of hiding
            stage.push(dsp::toggle_pin(addr));
        }
        stage.push(dsp::to_workspace(addr, &conf.workspace()));
        hypr.eval(&stage).await?;
        if conf.animation.is_some() {
            tokio::time::sleep(ONE_FRAME).await;
            hypr.eval(&[dsp::tag(addr, &format!("-{NOANIM_TAG}"))]).await?;
        }
        Ok(())
    }

    fn on_focus(&mut self, addr: &str, class: &str) {
        let Some(mine) = &self.addr else { return };
        let ours = addr == mine || (!addr.is_empty() && self.conf.class.as_ref().is_some_and(|p| p.matches(class)));
        if ours {
            self.unfocus_gen += 1;
            return;
        }
        if !self.conf.unfocus_hide || !self.shown {
            return;
        }
        if self.last_shown.is_some_and(|t| t.elapsed() < AFTER_SHOW_GRACE) {
            debug!("{}: focus left right after show, ignoring", self.conf.name);
            return;
        }
        self.unfocus_gen += 1;
        let epoch = self.unfocus_gen;
        let delay = self.conf.hysteresis;
        let Some(tx) = self.me.upgrade() else { return };
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Msg::UnfocusFire { epoch }).await;
        });
    }

    /// Start the command and return right away; `poll_spawn` picks the
    /// window up from `openwindow` events or the poll ticks.
    async fn launch(&mut self, want_visible: bool, press: u64) -> Result<()> {
        let Some(cmd) = self.conf.command.clone() else {
            self.fail("no matching window and no command to start one").await;
            return Ok(());
        };
        self.epoch += 1;
        let epoch = self.epoch;
        let log_path = self.shared.log_dir.join(format!("{}.log", self.conf.name));
        let child = File::create(&log_path)
            .and_then(|out| Ok((out.try_clone()?, out)))
            .and_then(|(out, err)| {
                let mut command = tokio::process::Command::new("sh");
                command.arg("-c").arg(&cmd).stdin(Stdio::null()).stdout(out).stderr(err);
                // own session: the app outlives the daemon and its signals
                unsafe {
                    command.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                command.spawn()
            });
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                self.fail(&format!("couldn't start `{cmd}`: {e}")).await;
                return Ok(());
            }
        };
        self.spawned_pid = child.id();
        info!("{}: launching `{cmd}` (pid {:?}, output in {})", self.conf.name, self.spawned_pid, log_path.display());
        self.spawn = Some(Spawn { epoch, started: Instant::now(), want_visible, press });

        let Some(tx) = self.me.upgrade() else { return Ok(()) };
        let exit_tx = tx.clone();
        tokio::spawn(async move {
            let (success, status) = match child.wait().await {
                Ok(s) => (s.success(), s.to_string()),
                Err(e) => (false, e.to_string()),
            };
            let _ = exit_tx.send(Msg::SpawnExited { epoch, success, status }).await;
        });
        let limit = self.conf.spawn_timeout + SPAWN_POLL * 2;
        tokio::spawn(async move {
            let started = Instant::now();
            while started.elapsed() < limit {
                tokio::time::sleep(SPAWN_POLL).await;
                match tx.try_send(Msg::SpawnTick { epoch }) {
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                    _ => continue,
                }
            }
        });
        Ok(())
    }

    async fn poll_spawn(&mut self) -> Result<()> {
        let clients = self.hypr().clients().await?;
        let Some(sp) = &self.spawn else { return Ok(()) };
        let (want, press, took, timeout) = (sp.want_visible, sp.press, sp.started.elapsed(), self.conf.spawn_timeout);
        match self.find(&clients) {
            Some(c) => {
                info!("{}: window appeared after {:.1}s", self.conf.name, took.as_secs_f64());
                self.spawn = None;
                if want {
                    let monitors = self.hypr().monitors().await?;
                    self.show(&c, &clients, focused(&monitors)?, press).await?;
                } else {
                    let mut stage = vec![];
                    if !c.floating {
                        stage.push(dsp::toggle_float(&c.address));
                    }
                    if c.pinned {
                        stage.push(dsp::toggle_pin(&c.address));
                    }
                    stage.push(dsp::to_workspace(&c.address, &self.conf.workspace()));
                    self.hypr().eval(&stage).await?;
                }
            }
            None if took > timeout => {
                self.spawn = None;
                self.fail(&format!("no window after {}s", timeout.as_secs())).await;
            }
            None => {}
        }
        Ok(())
    }

    async fn fail(&self, why: &str) {
        let log = self.shared.log_dir.join(format!("{}.log", self.conf.name));
        error!("{}: {why} (app output: {})", self.conf.name, log.display());
        self.hypr().notify_error(&format!("{}: {why}", self.conf.name)).await;
    }
}

fn focused(monitors: &[Monitor]) -> Result<&Monitor> {
    monitors.iter().find(|m| m.focused).or(monitors.first()).ok_or_else(|| anyhow!("no monitors"))
}
