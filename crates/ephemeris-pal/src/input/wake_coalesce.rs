//! Coalesce event-loop wakes: at most one wake until the consumer clears.
//!
//! Used by the Linux evdev pen thread so a burst of samples does not flood
//! winit with `PenReady` user events (each wake costs a trip out of `Wait`).

use std::sync::atomic::{AtomicBool, Ordering};

/// Atomic "wake already requested" flag shared between producer and consumer.
#[derive(Debug, Default)]
pub struct WakeCoalesce {
    pending: AtomicBool,
}

impl WakeCoalesce {
    pub fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
        }
    }

    /// Invoke `wake` at most once until [`clear`](Self::clear).
    ///
    /// Returns `true` when this call performed the wake.
    pub fn request(&self, wake: impl FnOnce()) -> bool {
        if self
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            wake();
            true
        } else {
            false
        }
    }

    /// Clear after draining so the next sample may wake again.
    ///
    /// Call **before** collecting events so a concurrent emit after clear
    /// re-arms a wake for any samples that arrive mid-drain.
    pub fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn wakes_once_until_cleared() {
        let c = WakeCoalesce::new();
        let n = Arc::new(AtomicUsize::new(0));

        assert!(c.request(|| {
            n.fetch_add(1, Ordering::Relaxed);
        }));
        assert!(!c.request(|| {
            n.fetch_add(1, Ordering::Relaxed);
        }));
        assert!(!c.request(|| {
            n.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(n.load(Ordering::Relaxed), 1);
        assert!(c.is_pending());

        c.clear();
        assert!(!c.is_pending());
        assert!(c.request(|| {
            n.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }
}
