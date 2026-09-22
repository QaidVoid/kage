//! Lock helpers with the workspace-wide poison policy: recover.
//!
//! A panic in one thread poisons the mutex or `RwLock` it held. The
//! guard data is not necessarily inconsistent for every future reader,
//! and for a long-lived terminal UI the alternative - panicking the
//! render loop or silently dropping every later event - is worse than
//! continuing. Every lock acquisition in the workspace goes through
//! these helpers, which recover from poisoning via
//! [`std::sync::PoisonError::into_inner`] and never panic.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock `mutex`, recovering from a poisoned guard instead of panicking.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read-lock `rw`, recovering from a poisoned guard instead of panicking.
pub fn read<T: ?Sized>(rw: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    rw.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write-lock `rw`, recovering from a poisoned guard instead of panicking.
pub fn write<T: ?Sized>(rw: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    rw.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_recovers_from_poison() {
        let mutex = Mutex::new(1usize);
        std::thread::scope(|s| {
            let guard = mutex.lock().unwrap();
            let handle = s.spawn(|| {
                let _guard = mutex.lock().unwrap();
                panic!("poison the mutex");
            });
            drop(guard);
            assert!(handle.join().is_err());
        });
        assert_eq!(*lock(&mutex), 1);
    }

    #[test]
    fn read_and_write_recover_from_poison() {
        let rw = RwLock::new(String::from("ok"));
        std::thread::scope(|s| {
            let guard = rw.write().unwrap();
            let handle = s.spawn(|| {
                let _guard = rw.write().unwrap();
                panic!("poison the rwlock");
            });
            drop(guard);
            assert!(handle.join().is_err());
        });
        assert_eq!(*read(&rw), "ok");
        *write(&rw) = "still writable".to_owned();
        assert_eq!(*read(&rw), "still writable");
    }
}
