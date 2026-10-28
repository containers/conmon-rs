//! Lock helpers.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock a mutex, also if a panic poisoned it. A panic aborts release builds,
/// so this only matters for tests and debug builds. Treating a poisoned lock
/// like an empty one there would hide the state behind it, like the children
/// which are waited for.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lock a read write lock for reading, also if a panic poisoned it, see
/// [`lock`].
pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Lock a read write lock for writing, also if a panic poisoned it, see
/// [`lock`].
pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
