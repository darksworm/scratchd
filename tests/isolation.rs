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
    /// Added to every query, like a busy compositor.
    latency: Duration,
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
                    let latency = st.lock().unwrap().latency;
                    if req.starts_with("j/") && !latency.is_zero() {
                        tokio::time::sleep(latency).await;
                    }
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

    fn workspace(&self, addr: &str) -> String {
        let s = self.state.lock().unwrap();
        let c = s.clients.iter().find(|c| c["address"] == addr).expect("client exists");
        c["workspace"]["name"].as_str().unwrap().to_owned()
    }

    fn set_workspace(&self, addr: &str, ws: &str) {
        let mut s = self.state.lock().unwrap();
        for c in s.clients.iter_mut().filter(|c| c["address"] == addr) {
            c["workspace"]["name"] = json!(ws);
        }
    }

    /// Wait until no evals arrive for a while.
    async fn settle(&self) {
        let mut last = usize::MAX;
        loop {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let n = self.state.lock().unwrap().evals.len();
            if n == last {
                return;
            }
            last = n;
        }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_key_pressed_wins_when_two_exclusive_scratchpads_race() {
    let fake = Fake::start(
        r#"
[scratchpads.a]
command = "true"
class = "a"
excludes = "*"

[scratchpads.b]
command = "true"
class = "b"
excludes = "*"
"#,
        vec![client("0xaa", "a", "special:S-a"), client("0xbb", "b", "special:S-b")],
    )
    .await;
    // slow enough that both actors are mid-show when the other's exclusion arrives
    fake.state.lock().unwrap().latency = Duration::from_millis(30);

    for (first, second) in [("a", "b"), ("b", "a"), ("a", "b"), ("b", "a"), ("a", "b")] {
        fake.set_workspace("0xaa", "special:S-a");
        fake.set_workspace("0xbb", "special:S-b");
        // both presses land before either actor has finished showing
        let (cmd1, cmd2) = (format!("toggle {first}"), format!("toggle {second}"));
        let (r1, r2) = tokio::join!(fake.ctl(&cmd1), async {
            tokio::time::sleep(Duration::from_millis(2)).await;
            fake.ctl(&cmd2).await
        });
        assert_eq!((r1.as_str(), r2.as_str()), ("ok", "ok"));
        fake.settle().await;
        let addr = |n: &str| if n == "a" { "0xaa" } else { "0xbb" };
        assert_eq!(fake.workspace(addr(second)), "1", "{second} was pressed last and should be showing");
        assert_eq!(fake.workspace(addr(first)), format!("special:S-{first}"), "{first} should have been hidden");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_windows_of_the_app_hide_and_return_with_it() {
    let mut call = client("0xca11", "app", "1");
    call["at"] = json!([300, 250]);
    let fake = Fake::start(
        r#"
[scratchpads.app]
command = "true"
class = "app"
position = "10% 10%"
animation = ""

[scratchpads.solo]
command = "true"
class = "solo"
multi = false
"#,
        vec![
            client("0xa1", "app", "1"),
            call,
            client("0x07e2", "other", "1"),
            client("0x5010", "solo", "1"),
            client("0x5011", "solo", "1"),
        ],
    )
    .await;

    fake.ctl("toggle app").await;
    fake.settle().await;
    assert_eq!(fake.workspace("0xa1"), "special:S-app");
    assert_eq!(fake.workspace("0xca11"), "special:S-app", "the app's second window should hide too");
    assert_eq!(fake.workspace("0x07e2"), "1", "unrelated windows stay");

    fake.ctl("toggle app").await;
    fake.settle().await;
    assert_eq!(fake.workspace("0xa1"), "1");
    assert_eq!(fake.workspace("0xca11"), "1");
    // main lands at 10% of 2560x1440 = (256, 144); the call keeps its (200, 150) offset
    let moved = r#"hl.dsp.window.move({x=456, y=294, window="address:0xca11"})"#;
    assert!(fake.state.lock().unwrap().evals.iter().any(|e| e.contains(moved)));

    fake.ctl("toggle solo").await;
    fake.settle().await;
    assert_eq!(fake.workspace("0x5010"), "special:S-solo");
    assert_eq!(fake.workspace("0x5011"), "1", "multi = false leaves other windows alone");
}
