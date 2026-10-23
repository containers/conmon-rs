//! Mutex helpers.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock a mutex, also if a panic poisoned it. A panic aborts release builds,
/// so this only matters for tests and debug builds. Treating a poisoned lock
/// like an empty one there would hide the state behind it, like the children
/// which are waited for.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
