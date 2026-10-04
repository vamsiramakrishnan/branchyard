use std::panic::Location;
use std::sync::{
    Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
    WaitTimeoutResult,
};
use std::time::Duration;

fn poisoned(name: &str, at: &Location<'_>) {
    tracing::warn!(
        lock = %name,
        at = %at,
        "lock `{name}` was poisoned by a panic in another thread; recovering its data (taken at {at})"
    );
}

fn recover<G>(
    name: &str,
    at: &Location<'_>,
    result: Result<G, PoisonError<G>>,
    clear: impl FnOnce(),
) -> G {
    match result {
        Ok(guard) => guard,
        Err(error) => {
            poisoned(name, at);
            // Clear the flag so the next holder does not log it again: the
            // warning is for the panic, not for every later use.
            clear();
            error.into_inner()
        }
    }
}

/// Take a `Mutex` whose holder may have panicked.
///
/// std marks a lock poisoned when its holder panics. Every lock in this
/// workspace guards state that stays usable (a counter, a map, a connection),
/// so the right response is to go on, but visibly: the first caller after the
/// panic logs a warning naming the lock, and the poison flag is cleared so
/// later callers stay quiet.
///
/// Use these instead of `.lock().unwrap_or_else(|e| e.into_inner())`; a test
/// in this crate fails when that idiom reappears.
pub trait LockExt<T> {
    /// Lock, recovering from poison. `name` says which lock, in the log.
    #[track_caller]
    fn lock_recovering(&self, name: &str) -> MutexGuard<'_, T>;

    /// Borrow the data of a lock this thread owns outright, recovering from
    /// poison.
    #[track_caller]
    fn get_mut_recovering(&mut self, name: &str) -> &mut T;

    /// Take the data out of a lock this thread owns outright, recovering
    /// from poison.
    #[track_caller]
    fn into_inner_recovering(self, name: &str) -> T
    where
        Self: Sized;
}

impl<T> LockExt<T> for Mutex<T> {
    #[track_caller]
    fn lock_recovering(&self, name: &str) -> MutexGuard<'_, T> {
        recover(name, Location::caller(), self.lock(), || {
            self.clear_poison()
        })
    }

    #[track_caller]
    fn get_mut_recovering(&mut self, name: &str) -> &mut T {
        if self.is_poisoned() {
            poisoned(name, Location::caller());
            self.clear_poison();
        }
        // `clear_poison` was just called, but a poisoned lock can still hand
        // its data back, which is all this needs.
        self.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    #[track_caller]
    fn into_inner_recovering(self, name: &str) -> T {
        if self.is_poisoned() {
            poisoned(name, Location::caller());
        }
        self.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`LockExt`] for `RwLock`.
pub trait RwLockExt<T> {
    #[track_caller]
    fn read_recovering(&self, name: &str) -> RwLockReadGuard<'_, T>;
    #[track_caller]
    fn write_recovering(&self, name: &str) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    #[track_caller]
    fn read_recovering(&self, name: &str) -> RwLockReadGuard<'_, T> {
        recover(name, Location::caller(), self.read(), || {
            self.clear_poison()
        })
    }

    #[track_caller]
    fn write_recovering(&self, name: &str) -> RwLockWriteGuard<'_, T> {
        recover(name, Location::caller(), self.write(), || {
            self.clear_poison()
        })
    }
}

/// Wait on a [`Condvar`] without failing on a poisoned mutex.
///
/// A guard cannot reach its mutex to clear the poison flag, so a wait that
/// meets poison logs each time until the next `lock_recovering` clears it.
pub trait CondvarExt {
    #[track_caller]
    fn wait_recovering<'a, T>(&self, guard: MutexGuard<'a, T>, name: &str) -> MutexGuard<'a, T>;

    #[track_caller]
    fn wait_timeout_recovering<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        name: &str,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult);

    #[track_caller]
    fn wait_timeout_while_recovering<'a, T, F: FnMut(&mut T) -> bool>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        condition: F,
        name: &str,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult);
}

impl CondvarExt for Condvar {
    #[track_caller]
    fn wait_recovering<'a, T>(&self, guard: MutexGuard<'a, T>, name: &str) -> MutexGuard<'a, T> {
        recover(name, Location::caller(), self.wait(guard), || {})
    }

    #[track_caller]
    fn wait_timeout_recovering<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        name: &str,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        recover(
            name,
            Location::caller(),
            self.wait_timeout(guard, timeout),
            || {},
        )
    }

    #[track_caller]
    fn wait_timeout_while_recovering<'a, T, F: FnMut(&mut T) -> bool>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration,
        condition: F,
        name: &str,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        recover(
            name,
            Location::caller(),
            self.wait_timeout_while(guard, timeout, condition),
            || {},
        )
    }
}
