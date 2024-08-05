# filedesc [![docs][docs-badge]][docs] [![tests][tests-badge]][tests]
[docs]: https://docs.rs/filedesc/
[tests]: https://github.com/de-vri-es/filedesc-rs/actions?query=workflow%3Atests
[docs-badge]: https://docs.rs/filedesc/badge.svg
[tests-badge]: https://github.com/de-vri-es/filedesc-rs/workflows/tests/badge.svg

This crate exposes a single type: [`FileDesc`][FileDesc],
which acts as a thin wrapper around open file descriptors.
The wrapped file descriptor is closed when the wrapper is dropped.

You can call [`FileDesc::new()`][FileDesc::new] with any type that implements [`IntoRawFd`][std::os::unix::io::IntoRawFd],
or duplicate the file descriptor of a type that implements [`AsRawFd`][std::os::unix::io::AsRawFd] with [`duplicate_from`][FileDesc::duplicate_from],
or directly from a raw file descriptor with [`from_raw_fd()`][FileDesc::from_raw_fd] and [`duplicate_raw_fd()`][FileDesc::duplicate_raw_fd].
Wrapped file descriptors can also be duplicated with the [`duplicate()`][FileDesc::duplicate] function.

## Close-on-exec
Whenever the library duplicates a file descriptor, it tries to set the `close-on-exec` flag atomically.
On platforms where this is not supported, the library falls back to setting the flag non-atomically.
When an existing file descriptor is wrapped, the `close-on-exec` flag is left as it was.

You can also check or set the `close-on-exec` flag with the [`get_close_on_exec()`][FileDesc::get_close_on_exec]
and [`set_close_on_exec`][FileDesc::set_close_on_exec] functions.

## Example
```rust
use filedesc::FileDesc;
let fd = unsafe { FileDesc::from_raw_fd(raw_fd) };
let duplicated = unsafe { fd.duplicate()? };
assert_eq!(duplicated.get_close_on_exec()?, true);

duplicated.set_close_on_exec(false)?;
assert_eq!(duplicated.get_close_on_exec()?, false);
```

[FileDesc]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html
[FileDesc::duplicate]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.duplicate
[FileDesc::duplicate_from]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.duplicate_from
[FileDesc::duplicate_raw_fd]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.duplicate_raw_fd
[FileDesc::from_raw_fd]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.from_raw_fd
[FileDesc::get_close_on_exec]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.get_close_on_exec
[FileDesc::new]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.new
[FileDesc::set_close_on_exec]: https://docs.rs/filedesc/latest/filedesc/struct.FileDesc.html#method.set_close_on_exec
[std::os::unix::io::AsRawFd]: https://doc.rust-lang.org/stable/std/os/unix/io/trait.AsRawFd.html
[std::os::unix::io::IntoRawFd]: https://doc.rust-lang.org/stable/std/os/unix/io/trait.IntoRawFd.html
