//! `masstuffy-server`: the HTTP API over one Masstuffy repository directory.
//!
//! There is no authentication or TLS: bind to loopback, or put an authenticating reverse proxy in
//! front. Exit codes: 0 clean shutdown, 1 runtime failure, 2 usage error.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use masstuffy::Repository;
use masstuffy_http::{HttpOptions, ServeOptions, Stop, router, serve};
use tokio::net::TcpListener;
use tokio::sync::watch;

const USAGE: &str =
    "usage: masstuffy-server <WORKDIR> [--listen ADDR] [--init] [--max-body-bytes N]
                        [--max-connections N] [--max-concurrent-requests N]
                        [--request-timeout SECS] [--drain-timeout SECS]
                        [--log error|warn|info|debug|trace] [--help] [--version]";

struct Args {
    workdir: PathBuf,
    listen: SocketAddr,
    init: bool,
    http: HttpOptions,
    serve: ServeOptions,
    log: tracing::Level,
}

enum Command {
    Run(Box<Args>),
    Help,
    Version,
}

fn positive(flag: &str, v: &str) -> Result<u64, String> {
    match v.parse::<u64>() {
        Ok(n) if n > 0 && !v.starts_with('+') => Ok(n),
        _ => Err(format!(
            "{flag}: expected a positive decimal number, got {v:?}"
        )),
    }
}

fn size(flag: &str, v: &str) -> Result<usize, String> {
    usize::try_from(positive(flag, v)?).map_err(|_| format!("{flag}: value too large"))
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut it = args.into_iter();
    let mut workdir: Option<PathBuf> = None;
    let mut out = Args {
        workdir: PathBuf::new(),
        listen: SocketAddr::from(([127, 0, 0, 1], 8470)),
        init: false,
        http: HttpOptions::default(),
        serve: ServeOptions::default(),
        log: tracing::Level::INFO,
    };
    let mut seen: Vec<String> = Vec::new();
    while let Some(arg) = it.next() {
        let Some(text) = arg.to_str() else {
            if workdir.is_some() {
                return Err("only one WORKDIR is accepted".into());
            }
            workdir = Some(PathBuf::from(arg));
            continue;
        };
        if !text.starts_with("--") {
            if workdir.is_some() {
                return Err("only one WORKDIR is accepted".into());
            }
            workdir = Some(PathBuf::from(text));
            continue;
        }
        let (flag, inline) = match text.split_once('=') {
            Some((f, v)) => (f, Some(v.to_owned())),
            None => (text, None),
        };
        if seen.iter().any(|s| s == flag) {
            return Err(format!("duplicate flag {flag}"));
        }
        seen.push(flag.to_owned());
        match flag {
            "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "--init" => {
                if inline.is_some() {
                    return Err("--init takes no value".into());
                }
                out.init = true;
                continue;
            }
            "--listen"
            | "--max-body-bytes"
            | "--max-connections"
            | "--max-concurrent-requests"
            | "--request-timeout"
            | "--drain-timeout"
            | "--log" => {}
            _ => return Err(format!("unknown flag {flag}")),
        }
        let value = match inline {
            Some(v) => v,
            None => it
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?
                .into_string()
                .map_err(|_| format!("{flag}: value is not UTF-8"))?,
        };
        match flag {
            "--listen" => {
                out.listen = value
                    .parse()
                    .map_err(|_| format!("--listen: invalid address {value:?}"))?;
            }
            "--max-body-bytes" => {
                let n = positive(flag, &value)?;
                out.http.max_put_body_bytes = n;
                out.http.max_import_body_bytes = Some(n);
            }
            "--max-connections" => out.serve.max_connections = size(flag, &value)?,
            "--max-concurrent-requests" => out.http.max_concurrent_requests = size(flag, &value)?,
            "--request-timeout" => {
                out.http.request_timeout = Duration::from_secs(positive(flag, &value)?);
            }
            "--drain-timeout" => {
                out.serve.drain_timeout = Duration::from_secs(positive(flag, &value)?);
            }
            "--log" => {
                out.log = match value.as_str() {
                    "error" => tracing::Level::ERROR,
                    "warn" => tracing::Level::WARN,
                    "info" => tracing::Level::INFO,
                    "debug" => tracing::Level::DEBUG,
                    "trace" => tracing::Level::TRACE,
                    _ => return Err(format!("--log: unknown level {value:?}")),
                };
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    out.workdir = workdir.ok_or("missing WORKDIR")?;
    Ok(Command::Run(Box::new(out)))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Run(a)) => *a,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("masstuffy-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_max_level(args.log)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("cannot start the runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(1)
        }
    }
}

/// Waits for SIGINT/SIGTERM: the first drains, the second stops at once.
#[cfg(unix)]
fn spawn_signals(tx: watch::Sender<Stop>) -> Result<(), std::io::Error> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        for n in 0..3 {
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
            match n {
                0 => {
                    tracing::info!("signal received: draining (send it again to stop at once)");
                    let _ = tx.send(Stop::Drain);
                }
                1 => {
                    tracing::warn!("second signal: stopping now (send it again to exit at once)");
                    let _ = tx.send(Stop::Now);
                }
                _ => {
                    // The engine shutdown is stuck or too slow: recovery at the next open
                    // repairs whatever was in flight (spec ch. 5).
                    tracing::error!("third signal: exiting without waiting");
                    std::process::exit(130);
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_signals(tx: watch::Sender<Stop>) -> Result<(), std::io::Error> {
    tokio::spawn(async move {
        for n in 0..3 {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            match n {
                0 => {
                    let _ = tx.send(Stop::Drain);
                }
                1 => {
                    let _ = tx.send(Stop::Now);
                }
                _ => std::process::exit(130),
            }
        }
    });
    Ok(())
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let wd = &args.workdir;
    let (repo, report) = if args.init && (!wd.exists() || !wd.join("config.json").exists()) {
        (Repository::init(wd).await?, None)
    } else {
        let (repo, report) = Repository::open(wd).await?;
        tracing::info!(loaded = report.loaded.len(), "repository opened");
        for s in &report.skipped {
            tracing::warn!(dir = %s.dir.display(), reason = ?s.reason, "collection skipped");
        }
        if report.index_recreated {
            tracing::warn!("secondary index recreated; collections are being re-indexed");
        }
        (repo, Some(report))
    };
    let prepared = prepare(&args, &repo, report.as_ref()).await;
    let (listener, app, tx, rx) = match prepared {
        Ok(p) => p,
        Err(e) => {
            if let Err(se) = repo.shutdown().await {
                tracing::error!("shutdown failed: {se}");
            }
            return Err(e);
        }
    };
    let _ = tx; // kept alive by the signal task
    let served = serve(listener, app, args.serve, rx).await;
    let shut = repo.shutdown().await;
    served?;
    shut?;
    tracing::info!("clean shutdown");
    Ok(())
}

type Prepared = (
    TcpListener,
    axum::Router,
    watch::Sender<Stop>,
    watch::Receiver<Stop>,
);

async fn prepare(
    args: &Args,
    repo: &Repository,
    report: Option<&masstuffy::OpenReport>,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    args.http.validate()?;
    args.serve.validate()?;
    let listener = TcpListener::bind(args.listen).await?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        tracing::warn!(
            "listening on a non-loopback address without authentication; put an authenticating reverse proxy in front"
        );
    }
    tracing::info!("listening on {addr}");
    let (tx, rx) = watch::channel(Stop::Run);
    spawn_signals(tx.clone())?;
    let app = router(repo.clone(), args.http.clone(), report)?;
    Ok((listener, app, tx, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Command, String> {
        parse_args(a.iter().map(OsString::from))
    }

    fn run_args(a: &[&str]) -> Args {
        match p(a).unwrap() {
            Command::Run(a) => *a,
            _ => panic!("not a run command"),
        }
    }

    #[test]
    fn parse_defaults() {
        let a = run_args(&["/w"]);
        assert_eq!(a.workdir, PathBuf::from("/w"));
        assert_eq!(a.listen, "127.0.0.1:8470".parse().unwrap());
        assert!(!a.init);
        assert_eq!(a.log, tracing::Level::INFO);
        assert_eq!(
            a.http.max_put_body_bytes,
            HttpOptions::default().max_put_body_bytes
        );
        assert_eq!(a.serve.max_connections, 256);
    }

    #[test]
    fn parse_all_flags() {
        let a = run_args(&[
            "/w",
            "--listen",
            "0.0.0.0:9",
            "--init",
            "--max-body-bytes",
            "100",
            "--max-connections",
            "5",
            "--max-concurrent-requests",
            "6",
            "--request-timeout",
            "7",
            "--drain-timeout",
            "8",
            "--log",
            "debug",
        ]);
        let b = run_args(&[
            "--listen=0.0.0.0:9",
            "/w",
            "--init",
            "--max-body-bytes=100",
            "--max-connections=5",
            "--max-concurrent-requests=6",
            "--request-timeout=7",
            "--drain-timeout=8",
            "--log=debug",
        ]);
        for a in [a, b] {
            assert_eq!(a.listen, "0.0.0.0:9".parse().unwrap());
            assert!(a.init);
            assert_eq!(a.http.max_put_body_bytes, 100);
            assert_eq!(a.http.max_import_body_bytes, Some(100));
            assert_eq!(a.serve.max_connections, 5);
            assert_eq!(a.http.max_concurrent_requests, 6);
            assert_eq!(a.http.request_timeout, Duration::from_secs(7));
            assert_eq!(a.serve.drain_timeout, Duration::from_secs(8));
            assert_eq!(a.log, tracing::Level::DEBUG);
        }
    }

    #[test]
    fn parse_errors() {
        for bad in [
            &[][..],
            &["/a", "/b"],
            &["/w", "--nope"],
            &["/w", "--init", "--init"],
            &["/w", "--listen"],
            &["/w", "--max-body-bytes", "0"],
            &["/w", "--max-body-bytes", "+5"],
            &["/w", "--listen", "nope"],
            &["/w", "--log", "loud"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parse_help_version() {
        assert!(matches!(p(&["--help"]), Ok(Command::Help)));
        assert!(matches!(p(&["/w", "--version"]), Ok(Command::Version)));
    }
}
