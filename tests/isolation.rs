//! Runs the daemon against a fake Hyprland to check that one misbehaving app
//! can't hold up the others.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use scratchd::daemon::{self, Options};
use scratchd::hypr::Hypr;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[derive(Default)]
struct State {
    clients: Vec<Value>,
    evals: Vec<String>,
    notifies: Vec<String>,
    events: Option<UnixStream>,
}

struct Fake {
    dir: tempfile::TempDir,
    state: Arc<Mutex<State>>,
}

fn client(addr: &str, class: &str, workspace: &str) -> Value {
    json!({
        "address": addr, "mapped": true, "at": [100, 100], "size": [800, 600],
        "workspace": {"id": 1, "name": workspace}, "floating": true, "pinned": false,
        "monitor": 0, "class": class, "title": class, "initialClass": class,
        "initialTitle": class, "pid": 999999
    })
}

impl Fake {
    async fn start(config: &str, clients: Vec<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let state = Arc::new(Mutex::new(State { clients, ..Default::default() }));

        let ctl = UnixListener::bind(dir.path().join(".socket.sock")).unwrap();
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = ctl.accept().await.unwrap();
                let st = st.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0; 65536];
                    let n = s.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let reply = respond(&st, &req);
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        let ev = UnixListener::bind(dir.path().join(".socket2.sock")).unwrap();
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let (s, _) = ev.accept().await.unwrap();
                st.lock().unwrap().events = Some(s);
            }
        });

        let opts = Options {
            config: dir.path().join("config.toml"),
            socket: dir.path().join("ctl.sock"),
            log_dir: dir.path().join("logs"),
        };
        tokio::spawn(daemon::run(Hypr::new(dir.path().to_owned()), opts));
        let fake = Self { dir, state };
        fake.wait_for(Duration::from_secs(2), |s| s.events.is_some()).await;
        fake
    }

    fn sock(&self) -> PathBuf {
        self.dir.path().join("ctl.sock")
    }

    async fn ctl(&self, line: &str) -> String {
        ctl(&self.sock(), line).await
    }

    async fn event(&self, line: &str) {
        let stream = self.state.lock().unwrap().events.take();
        let mut stream = stream.expect("daemon connected to events");
        stream.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        self.state.lock().unwrap().events = Some(stream);
    }

    async fn wait_for(&self, limit: Duration, f: impl Fn(&State) -> bool) -> Duration {
        let start = Instant::now();
        while start.elapsed() < limit {
            if f(&self.state.lock().unwrap()) {
                return start.elapsed();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let s = self.state.lock().unwrap();
        panic!("condition not met in {limit:?}\nevals: {:#?}\nnotifies: {:#?}", s.evals, s.notifies);
    }
}

async fn ctl(path: &Path, line: &str) -> String {
    for _ in 0..100 {
        if let Ok(mut s) = UnixStream::connect(path).await {
            s.write_all(line.as_bytes()).await.unwrap();
            s.shutdown().await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            return out.trim().to_owned();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket never came up");
}

fn respond(st: &Mutex<State>, req: &str) -> String {
    let mut st = st.lock().unwrap();
    match req {
        "j/clients" => serde_json::to_string(&st.clients).unwrap(),
        "j/monitors" => json!([{
            "id": 0, "name": "DP-3", "x": 0, "y": 0, "width": 3840, "height": 2160, "scale": 1.5,
            "transform": 0, "focused": true, "activeWorkspace": {"name": "1"}, "specialWorkspace": {"name": ""}
        }])
        .to_string(),
        "j/activewindow" => "{}".into(),
        _ if req.starts_with("/eval ") => {
            let code = req["/eval ".len()..].to_owned();
            // keep client workspaces in step with the moves the daemon asks for
            let re = Regex::new(r#"workspace="([^"]+)", follow=false, window="address:([^"]+)""#).unwrap();
            for cap in re.captures_iter(&code) {
                for c in st.clients.iter_mut().filter(|c| c["address"] == cap[2]) {
                    c["workspace"]["name"] = json!(cap[1].to_owned());
                }
            }
            st.evals.push(code);
            "ok".into()
        }
        _ if req.starts_with("/notify ") => {
            st.notifies.push(req.to_owned());
            "ok".into()
        }
        _ => "unknown request".into(),
    }
}

fn shown_on_ws1(s: &State, addr: &str) -> bool {
    let needle = format!(r#"workspace="1", follow=false, window="address:{addr}""#);
    s.evals.iter().any(|e| e.contains(&needle))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_app_does_not_delay_another_scratchpad() {
    let fake = Fake::start(
        r#"
[scratchpads.stuck]
command = "sleep 3"
class = "never-appears"

[scratchpads.fast]
command = "true"
class = "fast"
excludes = "*"
"#,
        vec![client("0xfa57", "fast", "special:S-fast")],
    )
    .await;

    assert_eq!(fake.ctl("toggle stuck").await, "ok");
    assert_eq!(fake.ctl("toggle fast").await, "ok");
    let took = fake.wait_for(Duration::from_secs(2), |s| shown_on_ws1(s, "0xfa57")).await;
    assert!(took < Duration::from_millis(300), "fast took {took:?} while stuck was launching");

    let status: Value = serde_json::from_str(&fake.ctl("status").await).unwrap();
    // showing fast excluded stuck, so stuck should now stay hidden when it arrives
    assert_eq!(status["stuck"]["launching"]["show_when_ready"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_command_reports_at_once() {
    let fake = Fake::start(
        r#"
[scratchpads.broken]
command = "echo boom >&2; exit 3"
class = "nope"
"#,
        vec![],
    )
    .await;

    fake.ctl("toggle broken").await;
    let took = fake.wait_for(Duration::from_secs(2), |s| s.notifies.iter().any(|n| n.contains("exit status: 3"))).await;
    assert!(took < Duration::from_millis(500), "failure took {took:?} to report");
    let log = std::fs::read_to_string(fake.dir.path().join("logs/broken.log")).unwrap();
    assert_eq!(log.trim(), "boom");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_toggle_while_launching_leaves_it_hidden() {
    let fake = Fake::start(
        r#"
[scratchpads.late]
command = "sleep 3"
class = "late"
"#,
        vec![],
    )
    .await;

    fake.ctl("toggle late").await;
    fake.ctl("toggle late").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    fake.state.lock().unwrap().clients.push(client("0x1a7e", "late", "1"));
    fake.event("openwindow>>1a7e,1,late,late").await;

    let hidden = r#"workspace="special:S-late", follow=false, window="address:0x1a7e""#;
    fake.wait_for(Duration::from_secs(2), |s| s.evals.iter().any(|e| e.contains(hidden))).await;
    let s = fake.state.lock().unwrap();
    assert!(!s.evals.iter().any(|e| e.contains("hl.dsp.focus")), "should not have been shown: {:#?}", s.evals);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_scratchpad_is_an_error_not_a_hang() {
    let fake = Fake::start("[scratchpads.a]\ncommand = \"true\"\nclass = \"a\"\n", vec![]).await;
    let reply = fake.ctl("toggle nope").await;
    assert!(reply.starts_with("error:") && reply.contains("nope"), "{reply}");
}
