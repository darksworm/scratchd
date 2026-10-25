//! Hyprland IPC: one short-lived connection per request on `.socket.sock`,
//! a long-lived line stream on `.socket2.sock` for events.
//!
//! Every request carries a timeout, so a wedged compositor stalls one call,
//! never the daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Hypr {
    dir: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WsRef {
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    #[serde(default)]
    pub mapped: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    #[serde(default)]
    pub workspace: WsRef,
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub monitor: i64,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub initial_class: String,
    #[serde(default)]
    pub initial_title: String,
    #[serde(default)]
    pub pid: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    #[serde(default)]
    pub transform: i32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub active_workspace: WsRef,
    #[serde(default)]
    pub special_workspace: WsRef,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ActiveWindow {
    #[serde(default)]
    pub address: String,
}

impl Hypr {
    pub fn from_env() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .context("HYPRLAND_INSTANCE_SIGNATURE not set; is Hyprland running?")?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let primary = Path::new(&runtime).join("hypr").join(&sig);
        let dir = if primary.exists() { primary } else { Path::new("/tmp/hypr").join(&sig) };
        Ok(Self { dir })
    }

    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn event_socket(&self) -> PathBuf {
        self.dir.join(".socket2.sock")
    }

    async fn request(&self, msg: &str) -> Result<String> {
        let path = self.dir.join(".socket.sock");
        let fut = async {
            let mut stream = UnixStream::connect(&path).await?;
            stream.write_all(msg.as_bytes()).await?;
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
        };
        match timeout(REQUEST_TIMEOUT, fut).await {
            Ok(r) => r.with_context(|| format!("hyprland request {:?}", truncate(msg))),
            Err(_) => bail!("hyprland request timed out: {:?}", truncate(msg)),
        }
    }

    async fn json<T: DeserializeOwned>(&self, what: &str) -> Result<T> {
        let body = self.request(&format!("j/{what}")).await?;
        serde_json::from_str(&body).with_context(|| format!("parsing `{what}` reply"))
    }

    pub async fn clients(&self) -> Result<Vec<Client>> {
        self.json("clients").await
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        self.json("monitors").await
    }

    pub async fn active_window(&self) -> Result<ActiveWindow> {
        // `{}` when nothing is focused
        self.json("activewindow").await
    }

    /// Run Lua statements in one `eval`, so a step's commands land together.
    pub async fn eval(&self, stmts: &[String]) -> Result<()> {
        if stmts.is_empty() {
            return Ok(());
        }
        let code = stmts.join(" ");
        tracing::debug!("eval {code}");
        let reply = self.request(&format!("/eval {code}")).await?;
        let reply = reply.trim();
        if reply.starts_with("error") {
            bail!("eval failed: {reply} (code: {code})");
        }
        if reply != "ok" {
            tracing::debug!("eval reply: {reply}");
        }
        Ok(())
    }

    pub async fn notify_error(&self, msg: &str) {
        let msg = msg.replace('\n', " ");
        if let Err(e) = self.request(&format!("/notify 0 6000 rgb(ff1010)  scratchd: {msg}")).await {
            tracing::warn!("notify failed: {e:#}");
        }
    }
}

fn truncate(s: &str) -> &str {
    match s.char_indices().nth(120) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Lua string literal.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Builders for `hl.dispatch(...)` statements, addressed by window address.
pub mod dsp {
    use super::lua_str;

    fn win(addr: &str) -> String {
        lua_str(&format!("address:{addr}"))
    }

    pub fn move_to(addr: &str, x: i32, y: i32) -> String {
        format!("hl.dispatch(hl.dsp.window.move({{x={x}, y={y}, window={}}}))", win(addr))
    }

    pub fn resize(addr: &str, w: i32, h: i32) -> String {
        format!("hl.dispatch(hl.dsp.window.resize({{x={w}, y={h}, window={}}}))", win(addr))
    }

    pub fn to_workspace(addr: &str, ws: &str) -> String {
        format!(
            "hl.dispatch(hl.dsp.window.move({{workspace={}, follow=false, window={}}}))",
            lua_str(ws),
            win(addr)
        )
    }

    pub fn workspace_to_monitor(ws: &str, monitor: &str) -> String {
        format!(
            "hl.dispatch(hl.dsp.workspace.move({{workspace={}, monitor={}}}))",
            lua_str(ws),
            lua_str(monitor)
        )
    }

    pub fn raise(addr: &str) -> String {
        format!("hl.dispatch(hl.dsp.window.alter_zorder({{mode=\"top\", window={}}}))", win(addr))
    }

    pub fn focus(addr: &str) -> String {
        format!("hl.dispatch(hl.dsp.focus({{window={}}}))", win(addr))
    }

    /// Toggles; callers check the current pin state first.
    pub fn toggle_pin(addr: &str) -> String {
        format!("hl.dispatch(hl.dsp.window.pin({{window={}}}))", win(addr))
    }

    /// Toggles; callers check the current floating state first.
    pub fn toggle_float(addr: &str) -> String {
        format!("hl.dispatch(hl.dsp.window.float({{window={}}}))", win(addr))
    }

    pub fn tag(addr: &str, tag: &str) -> String {
        format!("hl.dispatch(hl.dsp.window.tag({{tag={}, window={}}}))", lua_str(tag), win(addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_strings_are_escaped() {
        assert_eq!(lua_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(lua_str("x\ny"), r#""x\ny""#);
    }

    #[test]
    fn dispatch_statements_match_pypr_output() {
        assert_eq!(
            dsp::move_to("0x1", 332, 100),
            r#"hl.dispatch(hl.dsp.window.move({x=332, y=100, window="address:0x1"}))"#
        );
        assert_eq!(
            dsp::to_workspace("0x1", "special:S-slack"),
            r#"hl.dispatch(hl.dsp.window.move({workspace="special:S-slack", follow=false, window="address:0x1"}))"#
        );
    }

    #[test]
    fn parses_hyprland_client_json() {
        let json = r#"[{"address":"0xabc","mapped":true,"hidden":false,"at":[10,20],"size":[300,400],
            "workspace":{"id":-92,"name":"special:S-bt"},"floating":true,"pinned":false,"monitor":1,
            "class":"blueman-manager","title":"Bluetooth","initialClass":"blueman-manager",
            "initialTitle":"Bluetooth Devices","pid":42,"xwayland":false}]"#;
        let clients: Vec<Client> = serde_json::from_str(json).unwrap();
        assert_eq!(clients[0].workspace.name, "special:S-bt");
        assert_eq!(clients[0].initial_title, "Bluetooth Devices");
    }
}
