use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cancellation flag shared between whoever started some work and the threads doing it.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// A flag nobody ever sets.
    pub fn never() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// `Err(Cancelled)` once cancelled, for `?`.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() { Err(Cancelled) } else { Ok(()) }
    }

    /// Sleeps up to `ms`, waking early (and saying so) when cancelled.
    pub fn sleep(&self, ms: u64) -> bool {
        let mut left = ms;
        while left > 0 {
            if self.is_cancelled() {
                return false;
            }
            let step = left.min(50);
            std::thread::sleep(std::time::Duration::from_millis(step));
            left -= step;
        }
        !self.is_cancelled()
    }
}

/// The work was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Whether an error is (or wraps) a cancellation.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}
