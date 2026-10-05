//! Drop guard for acquire-run-restore sequences in the plugin host.
//!
//! Regions that arm a watchdog budget, set the current-plugin slot, or
//! bump the autocmd exec depth must restore that state on every exit
//! path. The Lua host contains job panics with `catch_unwind`, so a
//! panic inside such a region unwinds past it and would otherwise leave
//! the acquired state leaked for the rest of the process.

/// Runs a restore closure when dropped, unless defused.
pub(crate) struct RestoreOnDrop<'a> {
    restore: Option<Box<dyn FnMut() + 'a>>,
}

impl<'a> RestoreOnDrop<'a> {
    /// Arm the guard: `restore` runs once when the guard drops, so a
    /// panic inside the guarded region still restores.
    pub(crate) fn arm(restore: impl FnMut() + 'a) -> Self {
        Self {
            restore: Some(Box::new(restore)),
        }
    }

    /// Defuse the guard after an explicit restore on the normal path.
    pub(crate) fn defuse(mut self) {
        self.restore = None;
    }
}

impl Drop for RestoreOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(mut restore) = self.restore.take() {
            restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Mutex;

    use kage_core::sync::lock;

    use super::*;

    #[test]
    fn restore_runs_on_drop_and_defuse_skips_it() {
        let slot = Mutex::new(0);
        {
            RestoreOnDrop::arm(|| {
                *lock(&slot) = 1;
            })
            .defuse();
        }
        assert_eq!(*lock(&slot), 0);
        drop(RestoreOnDrop::arm(|| {
            *lock(&slot) = 1;
        }));
        assert_eq!(*lock(&slot), 1);
    }

    #[test]
    fn restore_runs_when_the_region_panics() {
        let slot = Mutex::new(0);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = RestoreOnDrop::arm(|| {
                *lock(&slot) = 1;
            });
            panic!("boom");
        }));
        assert!(outcome.is_err());
        assert_eq!(*lock(&slot), 1);
    }
}
