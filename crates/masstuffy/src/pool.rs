//! Bounded pools of idle compression and decompression contexts.

use std::sync::{Arc, Mutex, PoisonError};

use masstuffy_collection::{GenContext, PrepareError, Preparer, ReadCtx};

/// Idle `Preparer`s of one collection, bounded. The mutex is held only for a `Vec` push or pop,
/// never during a prepare.
pub(crate) struct PreparerPool {
    idle: Mutex<Vec<Preparer>>,
    cap: usize,
}

impl PreparerPool {
    pub(crate) fn new(cap: usize) -> PreparerPool {
        PreparerPool {
            idle: Mutex::new(Vec::new()),
            cap,
        }
    }

    /// Pops an idle preparer whose context is `Arc::ptr_eq` to `ctx`, discarding idle ones that
    /// are not (they belong to a replaced context); else creates one.
    pub(crate) fn take(&self, ctx: &Arc<GenContext>) -> Result<Preparer, PrepareError> {
        {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            while let Some(p) = idle.pop() {
                if Arc::ptr_eq(p.context(), ctx) {
                    return Ok(p);
                }
            }
        }
        Preparer::new(Arc::clone(ctx))
    }

    /// Returns `p` if fewer than `cap` are idle, else drops it.
    pub(crate) fn give(&self, p: Preparer) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < self.cap {
            idle.push(p);
        }
    }
}

/// Idle `ReadCtx`s (decompression contexts do not depend on the dictionary), bounded.
pub(crate) struct CtxPool {
    idle: Mutex<Vec<ReadCtx>>,
    cap: usize,
}

impl CtxPool {
    pub(crate) fn new(cap: usize) -> CtxPool {
        CtxPool {
            idle: Mutex::new(Vec::new()),
            cap,
        }
    }

    pub(crate) fn take(&self) -> ReadCtx {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default()
    }

    pub(crate) fn give(&self, c: ReadCtx) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < self.cap {
            idle.push(c);
        }
    }
}
