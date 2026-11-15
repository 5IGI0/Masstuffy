//! `masstuffy-server`: the HTTP API over one Masstuffy repository directory.
//!
//! There is no authentication or TLS: bind to loopback, or put an authenticating reverse proxy in
//! front. Exit codes: 0 clean shutdown, 1 runtime failure, 2 usage error.

mod settings;

use std::process::ExitCode;

use masstuffy::Repository;
use masstuffy_http::{Stop, router, serve};
use settings::{Args, Command, OPTIONS};
use tokio::net::TcpListener;
use tokio::sync::watch;

fn main() -> ExitCode {
    let args = match settings::parse(std::env::args_os().skip(1), std::env::vars_os()) {
        Ok(Command::Run(a)) => *a,
        Ok(Command::Help) => {
            print!("{}", settings::help_text());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("masstuffy-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n{}", settings::USAGE);
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_max_level(args.settings.log)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    for w in &args.warnings {
        tracing::warn!("{w}");
    }
    for (opt, src) in OPTIONS.iter().zip(&args.sources) {
        tracing::debug!(option = opt.flag, value = %(opt.show)(&args.settings), source = ?src, "option");
    }
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
    let (repo, report) = if args.settings.init && (!wd.exists() || !wd.join("config.json").exists())
    {
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
    let served = serve(listener, app, args.settings.serve, rx).await;
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
    args.settings.http.validate()?;
    args.settings.serve.validate()?;
    let listener = TcpListener::bind(args.settings.listen).await?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        tracing::warn!(
            "listening on a non-loopback address without authentication; put an authenticating reverse proxy in front"
        );
    }
    tracing::info!("listening on {addr}");
    let (tx, rx) = watch::channel(Stop::Run);
    spawn_signals(tx.clone())?;
    let app = router(repo.clone(), args.settings.http.clone(), report)?;
    Ok((listener, app, tx, rx))
}
