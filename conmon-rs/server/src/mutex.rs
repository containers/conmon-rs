//! Mutex helpers.

use anyhow::{Result, anyhow};
use std::sync::{Mutex, MutexGuard};

/// Lock a mutex and convert a poisoned lock into an `anyhow::Error`, so that
/// callers can propagate it with `?` instead of unwrapping.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|e| anyhow!("{e}"))
}
