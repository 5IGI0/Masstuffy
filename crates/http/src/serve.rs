//! The accept loop with a connection cap and a graceful drain.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;

use crate::options::{OptionsError, ServeOptions};

/// Server lifecycle, commanded through a `watch` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Keep serving.
    Run,
    /// Stop accepting and let in-flight requests finish (up to `drain_timeout`).
    Drain,
    /// Stop now: abort the remaining connections.
    Now,
}

/// A connection permit and the result of `accept`.
type Accepted = (OwnedSemaphorePermit, io::Result<(TcpStream, SocketAddr)>);

/// Waits for a free connection slot, then for a connection. Cancel-safe: both awaits are, and a
/// permit taken before a cancelled accept is simply dropped.
async fn accept_one(listener: &TcpListener, conns: &Arc<Semaphore>) -> Option<Accepted> {
    let permit = Arc::clone(conns).acquire_owned().await.ok()?;
    Some((permit, listener.accept().await))
}

/// Accepts connections until `stop` leaves `Run`, then drains: stops accepting (the listener is
/// dropped), asks every connection to finish its in-flight request and close (hyper graceful
/// shutdown), and waits until all connections ended, `drain_timeout` elapsed, or `stop` becomes
/// `Now`; the remaining connection tasks are then aborted and joined. Never calls
/// `Repository::shutdown` (the caller does, after this returns): aborting is safe because engine
/// mutations survive the cancellation of their callers.
///
/// # Errors
///
/// `OptionsError` (invalid options), checked first. I/O errors of the listener are logged and do
/// not end the loop.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    opts: ServeOptions,
    mut stop: watch::Receiver<Stop>,
) -> Result<(), OptionsError> {
    opts.validate()?;
    let conns = Arc::new(Semaphore::new(opts.max_connections));
    let graceful = GracefulShutdown::new();
    let mut tasks: JoinSet<()> = JoinSet::new();
    while *stop.borrow() == Stop::Run {
        tokio::select! {
            biased;
            r = stop.changed() => {
                // A dropped sender counts as a drain request (never as `Now`).
                if r.is_err() {
                    break;
                }
            }
            accepted = accept_one(&listener, &conns) => {
                let Some((permit, res)) = accepted else { break };
                match res {
                    Ok((stream, _peer)) => {
                        let _ = stream.set_nodelay(true);
                        let conn = hyper::server::conn::http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(opts.header_read_timeout)
                            .max_buf_size(opts.max_head_bytes)
                            .keep_alive(true)
                            .serve_connection(
                                TokioIo::new(stream),
                                TowerToHyperService::new(app.clone()),
                            );
                        let conn = graceful.watch(conn);
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = conn.await {
                                tracing::debug!(error = %e, "connection ended with an error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        drop(permit);
                        // The one sleep of the server: EMFILE and similar errors give no
                        // readiness notification, and retrying at once would spin.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                // Keeps the set bounded by the number of live connections.
                while tasks.try_join_next().is_some() {}
            }
        }
    }
    drop(listener);
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(opts.drain_timeout) => {
            tracing::warn!("drain timeout reached; closing the remaining connections");
        }
        () = async {
            // A dropped sender is "no `Now`": the drain runs to its deadline.
            if stop.wait_for(|s| *s == Stop::Now).await.is_err() {
                std::future::pending::<()>().await;
            }
        } => {}
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
