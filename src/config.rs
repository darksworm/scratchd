//! Config: the `[scratchpads.<name>]` tables from pypr's `config.toml`.
//!
//! A broken scratchpad is reported and skipped; the others still load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::geometry::Animation;
use crate::hypr::Client;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct Raw {
    command: Option<String>,
    class: Option<String>,
    match_by: Option<String>,
    #[serde(rename = "initialClass")]
    initial_class: Option<String>,
    #[serde(rename = "initialTitle")]
    initial_title: Option<String>,
    title: Option<String>,
    animation: Option<String>,
    size: Option<String>,
    position: Option<String>,
    max_size: Option<String>,
    margin: Option<i32>,
    lazy: Option<bool>,
    pinned: Option<bool>,
    unfocus: Option<String>,
    hysteresis: Option<f64>,
    excludes: Option<OneOrMany>,
    hide_delay: Option<f64>,
    spawn_timeout: Option<f64>,
}

/// Accepted so pypr configs load cleanly; scratchd never tracks or kills
/// the apps it starts, so they have no effect.
const IGNORED_KEYS: &[&str] = &["process_tracking"];

const KNOWN_KEYS: &[&str] = &[
    "command", "class", "match_by", "initialClass", "initialTitle", "title", "animation", "size",
    "position", "max_size", "margin", "lazy", "pinned", "unfocus", "hysteresis", "excludes",
    "hide_delay", "spawn_timeout",
];

#[derive(Clone, Debug)]
pub enum Pattern {
    Exact(String),
    Regex(Regex),
}

impl Pattern {
    fn new(s: &str) -> Result<Self> {
        Ok(match s.strip_prefix("re:") {
            Some(re) => Self::Regex(Regex::new(re).with_context(|| format!("bad regex {re:?}"))?),
            None => Self::Exact(s.to_owned()),
        })
    }

    pub fn matches(&self, v: &str) -> bool {
        match self {
            Self::Exact(s) => s == v,
            Self::Regex(re) => re.is_match(v),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Matcher {
    /// The spawned process or one of its descendants owns the window.
    Pid,
    Class(Pattern),
    InitialClass(Pattern),
    Title(Pattern),
    InitialTitle(Pattern),
}

impl Matcher {
    /// Whether the matched property stays put for the window's lifetime, so a
    /// remembered address can be re-checked against it.
    pub fn is_stable(&self) -> bool {
        matches!(self, Self::Class(_) | Self::InitialClass(_) | Self::InitialTitle(_))
    }

    pub fn matches(&self, c: &Client, spawned_pid: Option<u32>) -> bool {
        match self {
            Self::Pid => spawned_pid.is_some_and(|p| is_descendant(c.pid, p)),
            Self::Class(p) => p.matches(&c.class),
            Self::InitialClass(p) => p.matches(&c.initial_class),
            Self::Title(p) => p.matches(&c.title),
            Self::InitialTitle(p) => p.matches(&c.initial_title),
        }
    }
}

/// Walk `/proc` parents from `pid` looking for `ancestor`.
fn is_descendant(pid: i64, ancestor: u32) -> bool {
    let mut pid = pid;
    for _ in 0..32 {
        if pid <= 1 {
            return false;
        }
        if pid == ancestor as i64 {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
        // ppid is the 2nd field after the `(comm)`, which may contain spaces
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { return false };
        match rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
            Some(ppid) => pid = ppid,
            None => return false,
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Excludes {
    None,
    All,
}

#[derive(Clone, Debug)]
pub struct ScratchConf {
    pub name: String,
    pub command: Option<String>,
    /// Window class, used by the unfocus check to keep the app's own popups
    /// from hiding it.
    pub class: Option<Pattern>,
    pub matcher: Matcher,
    pub animation: Option<Animation>,
    pub size: String,
    pub position: Option<String>,
    pub max_size: Option<String>,
    pub margin: i32,
    pub lazy: bool,
    pub pinned: bool,
    pub unfocus_hide: bool,
    pub hysteresis: Duration,
    pub excludes: Excludes,
    pub exclude_names: Vec<String>,
    pub hide_delay: Duration,
    pub spawn_timeout: Duration,
}

impl ScratchConf {
    pub fn workspace(&self) -> String {
        let escaped: String =
            self.name.chars().map(|c| if matches!(c, ':' | '/' | ' ') { '_' } else { c }).collect();
        format!("special:S-{escaped}")
    }

    pub fn excludes(&self, other: &str) -> bool {
        other != self.name && (self.excludes == Excludes::All || self.exclude_names.iter().any(|n| n == other))
    }

    fn from_raw(name: &str, raw: Raw) -> Result<Self> {
        let pattern = |key: &str, v: &Option<String>| -> Result<Pattern> {
            match v {
                Some(v) => Pattern::new(v),
                None => bail!("match_by = {key:?} but `{key}` isn't set"),
            }
        };
        // pypr matches by pid unless process tracking is off; pid matching
        // misses apps that re-exec through wrappers (snap, flatpak), so a
        // configured class wins when match_by isn't explicit.
        let match_by = raw.match_by.clone().unwrap_or_else(|| {
            if raw.class.is_some() { "class".into() } else { "pid".into() }
        });
        let matcher = match match_by.as_str() {
            "pid" => Matcher::Pid,
            "class" => Matcher::Class(pattern("class", &raw.class)?),
            "initialClass" => Matcher::InitialClass(pattern("initialClass", &raw.initial_class)?),
            "title" => Matcher::Title(pattern("title", &raw.title)?),
            "initialTitle" => Matcher::InitialTitle(pattern("initialTitle", &raw.initial_title)?),
            other => bail!("unknown match_by {other:?}"),
        };
        if raw.command.is_none() && matches!(matcher, Matcher::Pid) {
            bail!("needs a `command` or a `class` to find its window");
        }
        let (excludes, exclude_names) = match raw.excludes {
            None => (Excludes::None, vec![]),
            Some(OneOrMany::One(s)) if s == "*" => (Excludes::All, vec![]),
            Some(OneOrMany::One(s)) => (Excludes::None, vec![s]),
            Some(OneOrMany::Many(v)) if v.iter().any(|s| s == "*") => (Excludes::All, vec![]),
            Some(OneOrMany::Many(v)) => (Excludes::None, v),
        };
        let unfocus_hide = match raw.unfocus.as_deref() {
            None | Some("") => false,
            Some("hide") => true,
            Some(other) => bail!("unfocus must be \"hide\" or empty, got {other:?}"),
        };
        let secs = |v: Option<f64>, default: f64| Duration::from_secs_f64(v.unwrap_or(default).max(0.0));
        Ok(Self {
            name: name.to_owned(),
            command: raw.command,
            class: raw.class.as_deref().map(Pattern::new).transpose()?,
            matcher,
            animation: Animation::parse(raw.animation.as_deref().unwrap_or("fromTop"))?,
            size: raw.size.unwrap_or_else(|| "80% 80%".into()),
            position: raw.position,
            max_size: raw.max_size,
            margin: raw.margin.unwrap_or(60),
            lazy: raw.lazy.unwrap_or(true),
            pinned: raw.pinned.unwrap_or(true),
            unfocus_hide,
            hysteresis: secs(raw.hysteresis, 0.4),
            excludes,
            exclude_names,
            hide_delay: secs(raw.hide_delay, 0.2),
            spawn_timeout: secs(raw.spawn_timeout, 20.0),
        })
    }
}

pub struct Loaded {
    pub scratches: Vec<ScratchConf>,
    pub errors: Vec<String>,
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("scratchd").join("config.toml")
}

pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Loaded> {
    let doc: toml::Table = toml::from_str(text).context("config is not valid TOML")?;
    let mut errors = Vec::new();
    let mut scratches = Vec::new();
    let tables = match doc.get("scratchpads") {
        Some(toml::Value::Table(t)) => t.clone(),
        Some(_) => bail!("`scratchpads` must be a table"),
        None => toml::Table::new(),
    };
    let tables: BTreeMap<_, _> = tables.into_iter().collect();
    for (name, value) in tables {
        if let toml::Value::Table(t) = &value {
            for key in t.keys().filter(|k| !KNOWN_KEYS.contains(&k.as_str()) && !IGNORED_KEYS.contains(&k.as_str())) {
                errors.push(format!("[scratchpads.{name}] ignoring unsupported key `{key}`"));
            }
        }
        let parsed = value
            .try_into::<Raw>()
            .map_err(anyhow::Error::from)
            .and_then(|raw| ScratchConf::from_raw(&name, raw));
        match parsed {
            Ok(conf) => scratches.push(conf),
            Err(e) => errors.push(format!("[scratchpads.{name}] skipped: {e:#}")),
        }
    }
    Ok(Loaded { scratches, errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYPR: &str = r#"
[pyprland]
plugins = ["scratchpads"]

[scratchpads.slack]
command = "env -u XDG_CONFIG_HOME slack"
class = "slack"
size = "75% 87%"
position = "13% 7%"
lazy = true
excludes = ["*"]
unfocus = "hide"
process_tracking = false
pinned = false

[scratchpads.discord]
command = "vesktop"
class = "vesktop"
match_by = "initialTitle"
initialTitle = "Discord"

[scratchpads.broken]
command = "x"
animation = "sideways"

[scratchpads.odd]
command = "y"
multi = false
"#;

    #[test]
    fn reads_pypr_config_and_skips_broken_entries() {
        let loaded = parse(PYPR).unwrap();
        let names: Vec<_> = loaded.scratches.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["discord", "odd", "slack"]);
        assert!(loaded.errors.iter().any(|e| e.contains("broken") && e.contains("sideways")));
        assert!(loaded.errors.iter().any(|e| e.contains("`multi`")));

        let slack = loaded.scratches.iter().find(|s| s.name == "slack").unwrap();
        assert!(slack.unfocus_hide && !slack.pinned && slack.lazy);
        assert_eq!(slack.excludes, Excludes::All);
        assert!(slack.excludes("teams") && !slack.excludes("slack"));
        assert!(matches!(slack.matcher, Matcher::Class(_)));
        assert_eq!(slack.workspace(), "special:S-slack");

        let discord = loaded.scratches.iter().find(|s| s.name == "discord").unwrap();
        assert!(matches!(discord.matcher, Matcher::InitialTitle(_)));
        assert!(discord.pinned, "pinned defaults to true like pypr");
    }

    #[test]
    fn regex_patterns() {
        let p = Pattern::new("re:^[Ss]potify$").unwrap();
        assert!(p.matches("Spotify") && p.matches("spotify") && !p.matches("spotify2"));
    }

    #[test]
    fn match_by_requires_its_value() {
        let err = parse("[scratchpads.a]\ncommand = \"a\"\nmatch_by = \"title\"\n").unwrap();
        assert!(err.scratches.is_empty());
        assert!(err.errors[0].contains("title"));
    }

    #[test]
    fn descendant_of_self() {
        let me = std::process::id();
        assert!(is_descendant(me as i64, me));
        assert!(!is_descendant(me as i64, u32::MAX));
    }
}
