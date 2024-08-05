//! This crate exposes a single type: [`FileDesc`][FileDesc],
//! which acts as a thin wrapper around open file descriptors.
//! The wrapped file descriptor is closed when the wrapper is dropped.
//!
//! You can call [`FileDesc::new()`][FileDesc::new] with any type that implements [`IntoRawFd`][std::os::unix::io::IntoRawFd],
//! or duplicate the file descriptor of a type that implements [`AsRawFd`][std::os::unix::io::AsRawFd] with [`duplicate_from`][FileDesc::duplicate_from],
//! or directly from a raw file descriptor with [`from_raw_fd()`][FileDesc::from_raw_fd] and [`duplicate_raw_fd()`][FileDesc::duplicate_raw_fd].
//! Wrapped file descriptors can also be duplicated with the [`duplicate()`][FileDesc::duplicate] function.
//!
//! # Close-on-exec
//! Whenever the library duplicates a file descriptor, it tries to set the `close-on-exec` flag atomically.
//! On platforms where this is not supported, the library falls back to setting the flag non-atomically.
//! When an existing file descriptor is wrapped, the `close-on-exec` flag is left as it was.
//!
//! You can also check or set the `close-on-exec` flag with the [`get_close_on_exec()`][FileDesc::get_close_on_exec]
//! and [`set_close_on_exec`][FileDesc::set_close_on_exec] functions.
//!
//! # Example
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! # let raw_fd = 10;
//! use filedesc::FileDesc;
//! let fd = unsafe { FileDesc::from_raw_fd(raw_fd) };
//! let duplicated = fd.duplicate()?;
//! assert_eq!(duplicated.get_close_on_exec()?, true);
//!
//! duplicated.set_close_on_exec(false)?;
//! assert_eq!(duplicated.get_close_on_exec()?, false);
//! # Ok(())
//! # }
//! ```

#[cfg(not(unix))]
compile_error!("This crate only supports Unix platforms.");

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(all(unix, test))]
mod test;
