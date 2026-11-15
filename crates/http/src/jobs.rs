//! Registry of long-running admin jobs (rebuild, train, index rebuild).
//!
//! Lock: `Jobs::table` is a leaf `std::sync::Mutex`, held only for map edits and clones: no
//! `.await`, no I/O and no other lock while it is held. The `watch` notification is sent after it
//! is released.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use masstuffy::CollectionId;
use serde_json::Value;
use tokio::sync::{Semaphore, watch};

use crate::error::ApiError;
use crate::json::{JobJson, now_string};

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobKind {
    Rebuild,
    Train,
    RebuildIndex,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Rebuild => "rebuild",
            JobKind::Train => "train",
            JobKind::RebuildIndex => "rebuild_index",
        }
    }
}

struct Job {
    json: JobJson,
    done: Arc<watch::Sender<bool>>,
}

#[derive(Default)]
struct JobTable {
    jobs: BTreeMap<u64, Job>,
    finished: VecDeque<u64>,
}

/// The job registry. Memory is bounded by `max_running + max_retained` entries.
pub(crate) struct Jobs {
    table: Mutex<JobTable>,
    running: Arc<Semaphore>,
    next_id: AtomicU64,
    max_retained: usize,
}

impl Jobs {
    pub(crate) fn new(max_running: usize, max_retained: usize) -> Arc<Jobs> {
        Arc::new(Jobs {
            table: Mutex::new(JobTable::default()),
            running: Arc::new(Semaphore::new(max_running)),
            next_id: AtomicU64::new(1),
            max_retained,
        })
    }

    fn lock(&self) -> MutexGuard<'_, JobTable> {
        // A poisoned lock only means a panic elsewhere while editing plain maps: keep serving.
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers and spawns `fut`.
    ///
    /// # Errors
    ///
    /// 503 `too_many_jobs` when no running slot is free.
    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        kind: JobKind,
        collection: Option<(Arc<str>, CollectionId)>,
        fut: F,
    ) -> Result<JobJson, ApiError>
    where
        F: Future<Output = Result<Value, masstuffy::Error>> + Send + 'static,
    {
        let permit = Arc::clone(&self.running)
            .try_acquire_owned()
            .map_err(|_| ApiError::unavailable("too_many_jobs", "too many running jobs", 5))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let json = JobJson {
            id: id.to_string(),
            kind: kind.as_str(),
            collection: collection.as_ref().map(|(s, _)| s.to_string()),
            collection_id: collection.as_ref().map(|(_, c)| c.to_string()),
            state: "running",
            started_at: now_string(),
            finished_at: None,
            result: None,
            error: None,
        };
        let (done, _rx) = watch::channel(false);
        self.lock().jobs.insert(
            id,
            Job {
                json: json.clone(),
                done: Arc::new(done),
            },
        );
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            let outcome = match tokio::spawn(fut).await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(ApiError::from(e).object()),
                Err(e) => Err(ApiError::internal(format!("job task failed: {e}")).object()),
            };
            this.finish(id, outcome);
        });
        Ok(json)
    }

    fn finish(&self, id: u64, outcome: Result<Value, Value>) {
        let done = {
            let mut t = self.lock();
            let Some(job) = t.jobs.get_mut(&id) else {
                return;
            };
            job.json.finished_at = now_string();
            match outcome {
                Ok(v) => {
                    job.json.state = "succeeded";
                    job.json.result = Some(v);
                }
                Err(e) => {
                    job.json.state = "failed";
                    job.json.error = Some(e);
                }
            }
            let done = Arc::clone(&job.done);
            t.finished.push_back(id);
            while t.finished.len() > self.max_retained {
                if let Some(old) = t.finished.pop_front() {
                    t.jobs.remove(&old);
                }
            }
            done
        };
        done.send_replace(true);
    }

    /// The job and a receiver that turns `true` once it has finished.
    pub(crate) fn get(&self, id: u64) -> Option<(JobJson, watch::Receiver<bool>)> {
        let t = self.lock();
        let job = t.jobs.get(&id)?;
        Some((job.json.clone(), job.done.subscribe()))
    }

    /// Running jobs first (newest first), then finished jobs, newest first.
    pub(crate) fn list(&self) -> Vec<JobJson> {
        let t = self.lock();
        let mut out: Vec<JobJson> = t
            .jobs
            .values()
            .rev()
            .filter(|j| j.json.state == "running")
            .map(|j| j.json.clone())
            .collect();
        out.extend(
            t.finished
                .iter()
                .rev()
                .filter_map(|id| t.jobs.get(id))
                .map(|j| j.json.clone()),
        );
        out
    }

    /// Jobs currently running.
    pub(crate) fn running(&self) -> usize {
        self.lock()
            .jobs
            .values()
            .filter(|j| j.json.state == "running")
            .count()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::oneshot;

    use super::*;
    use crate::testutil::bounded;

    async fn wait_done(mut rx: watch::Receiver<bool>) {
        rx.wait_for(|d| *d).await.unwrap();
    }

    fn id_of(j: &JobJson) -> u64 {
        j.id.parse().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn job_success_and_wait() {
        bounded(async {
            let jobs = Jobs::new(2, 8);
            let (tx, rx) = oneshot::channel::<()>();
            let j = jobs
                .spawn(JobKind::Train, None, async move {
                    rx.await.unwrap();
                    Ok(json!({"x": 1}))
                })
                .unwrap();
            let id = id_of(&j);
            let (cur, done) = jobs.get(id).unwrap();
            assert_eq!(cur.state, "running");
            assert_eq!(jobs.running(), 1);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), wait_done(done.clone()))
                    .await
                    .is_err()
            );
            tx.send(()).unwrap();
            wait_done(done).await;
            let (cur, _) = jobs.get(id).unwrap();
            assert_eq!(cur.state, "succeeded");
            assert_eq!(cur.result, Some(json!({"x": 1})));
            assert!(cur.finished_at.is_some());
            assert_eq!(jobs.running(), 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn job_failure_stores_error_object() {
        bounded(async {
            let jobs = Jobs::new(2, 8);
            let j = jobs
                .spawn(JobKind::Rebuild, None, async {
                    Err(masstuffy::Error::RebuildInProgress(
                        CollectionId::from_u128(1),
                    ))
                })
                .unwrap();
            let (_, done) = jobs.get(id_of(&j)).unwrap();
            wait_done(done).await;
            let (cur, _) = jobs.get(id_of(&j)).unwrap();
            assert_eq!(cur.state, "failed");
            let e = cur.error.unwrap();
            assert_eq!(e["code"], "rebuild_in_progress");
            assert_eq!(e["kind"], "conflict");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn job_panic_is_internal() {
        bounded(async {
            let jobs = Jobs::new(2, 8);
            let j = jobs
                .spawn(JobKind::Rebuild, None, async {
                    panic!("boom");
                })
                .unwrap();
            let (_, done) = jobs.get(id_of(&j)).unwrap();
            wait_done(done).await;
            let (cur, _) = jobs.get(id_of(&j)).unwrap();
            assert_eq!(cur.state, "failed");
            assert_eq!(cur.error.unwrap()["code"], "internal");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn too_many_running() {
        bounded(async {
            let jobs = Jobs::new(1, 8);
            let (tx, rx) = oneshot::channel::<()>();
            let first = jobs
                .spawn(JobKind::Train, None, async move {
                    rx.await.unwrap();
                    Ok(json!({}))
                })
                .unwrap();
            let e = jobs
                .spawn(JobKind::Train, None, async { Ok(json!({})) })
                .unwrap_err();
            assert_eq!(
                (e.status.as_u16(), e.code, e.retry_after),
                (503, "too_many_jobs", Some(5))
            );
            let (_, done) = jobs.get(id_of(&first)).unwrap();
            tx.send(()).unwrap();
            wait_done(done).await;
            // The permit is released when the task ends, a moment after `done` fires.
            let mut second = None;
            for _ in 0..200 {
                match jobs.spawn(JobKind::Train, None, async { Ok(json!({})) }) {
                    Ok(j) => {
                        second = Some(j);
                        break;
                    }
                    Err(_) => tokio::task::yield_now().await,
                }
            }
            assert!(second.is_some());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retention_evicts_oldest_finished_only() {
        bounded(async {
            let jobs = Jobs::new(8, 2);
            let (hold_tx, hold_rx) = oneshot::channel::<()>();
            let running = jobs
                .spawn(JobKind::Train, None, async move {
                    hold_rx.await.unwrap();
                    Ok(json!({}))
                })
                .unwrap();
            let mut finished = Vec::new();
            for _ in 0..4 {
                let j = jobs
                    .spawn(JobKind::RebuildIndex, None, async { Ok(json!({})) })
                    .unwrap();
                let (_, done) = jobs.get(id_of(&j)).unwrap_or_else(|| unreachable!());
                wait_done(done).await;
                finished.push(id_of(&j));
            }
            let list = jobs.list();
            let ids: Vec<u64> = list.iter().map(id_of).collect();
            assert_eq!(ids, vec![id_of(&running), finished[3], finished[2]]);
            assert!(jobs.get(finished[0]).is_none());
            assert!(jobs.get(id_of(&running)).is_some());
            let _ = hold_tx.send(());
        })
        .await;
    }
}
