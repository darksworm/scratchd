use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use scratchd::{config, daemon, hypr::Hypr};

const USAGE: &str = "\
usage:
  scratchd daemon [--config FILE] [--log FILE] [-v]
  scratchd toggle|show|hide NAME...     (NAME may be * for show/hide)
  scratchd status|reload|ping|exit";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("daemon") => run_daemon(args.get(1..).unwrap_or_default()),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(_) => client(&args.join(" ")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scratchd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Synchronous on purpose: no runtime to boot, so the keybind round trip is
/// a connect, a write and a read.
fn client(line: &str) -> Result<()> {
    let hypr = Hypr::from_env()?;
    let path = daemon::socket_path(&hypr);
    let mut stream = UnixStream::connect(&path).with_context(|| format!("daemon not running? ({})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(line.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let reply = reply.trim_end();
    if let Some(err) = reply.strip_prefix("error: ") {
        bail!("{err}");
    }
    if reply != "ok" {
        println!("{reply}");
    }
    Ok(())
}

fn run_daemon(args: &[String]) -> Result<()> {
    let mut config = config::default_path();
    let mut log: Option<PathBuf> = None;
    let mut verbose = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => config = it.next().context("--config needs a path")?.into(),
            "--log" => log = Some(it.next().context("--log needs a path")?.into()),
            "-v" | "--verbose" => verbose = true,
            other => bail!("unknown option {other:?}\n{USAGE}"),
        }
    }
    init_logging(log.as_deref(), verbose)?;

    let hypr = Hypr::from_env()?;
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    let opts = daemon::Options { config, socket: daemon::socket_path(&hypr), log_dir: state.join("scratchd") };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(daemon::run(hypr, opts))
}

fn init_logging(file: Option<&std::path::Path>, verbose: bool) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if verbose { "scratchd=debug" } else { "scratchd=info" }));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    match file {
        Some(path) => {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log {}", path.display()))?;
            builder.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
        }
        None => builder.with_writer(std::io::stderr).init(),
    }
    Ok(())
}
