v0.7.1 - 2023-11-15:
  * Ensure proper alginment of control message buffer in the writer.
  * Fix compilation on Illumos and Solaris platforms.

v0.7.0 - 2023-03-03:
  * Fix `OwnedFileDescriptors` iteration.

v0.6.0 - 2023-03-03:
  * Rework ancillary message API.
  * Support I/O safety in ancillary message API.
  * Implement `Into<OwnedFd>` for `UnixSeqpacket` and `UnixSeqpacketListener`.

v0.5.6 - 2022-11-30:
  * Implement `AsFd` for `UnixSeqpacket` and `UnixSeqpacketListener`.
  * Implement `TryFrom<OwnedFd>` for `UnixSeqpacket` and `UnixSeqpacketListener`.

v0.5.5 - 2022-05-30:
  * Add `as_async_fd()` to facilitate low level access to the file descriptor.

v0.5.4 - 2021-10-18:
  * Fix sending ancillary data on non-Linux platforms.
  * Fix building of documentation on non-Linux platforms.

v0.5.3 - 2021-07-03:
  * Update dependencies.

v0.5.2 - 2021-06-12:
  * Add conversions to/from raw FDs for `UnixSeqpacketListener`.
  * Remove `socket2` dependency.
  * Fix compilation for several BSD targets.

v0.5.1 - 2021-06-08:
  * Upgrade to `socket2` 0.4.

v0.5.0 - 2021-01-27:
  * Report socket address as `PathBuf`.
  * Remove `UnixSeqpacket::local/remote_addr`, as they never contain useful information.

v0.4.5 - 2021-01-27:
  * Properly allow multiple tasks to call async function on the same socket (poll functions still only wake the last task).
  * Fix potential hang in `UnixSeqpacketListener::accept()`.

v0.4.4 - 2021-01-26:
  * Fix potential hangs in I/O functions.

v0.4.3 - 2021-01-12:
  * Fix compilation for `musl` targets.
  * Add conversions to/from raw file descriptors.

v0.4.2 - 2020-12-26:
  * Fix links in `README.md`.

v0.4.1 - 2020-12-25:
  * Regenerate `README.md` from library documentation.

v0.4.0 - 2020-12-25:
  * Make I/O functions take `&self` instead of `&mut self`.
  * Deprecate the `split()` API.

v0.3.1 - 2021-01-26:
  * Fix potential hangs in I/O functions (backported from 0.4.4).

v0.3.0 - 2020-11-06:
  * Update to tokio 0.3.2.
  * Report peer credentials with own `UCred` type since tokio made the construction private.

v0.2.1 - 2020-10-06:
  * Fix receiving of ancillary data.

v0.2.0 - 2020-09-29:
  * Add supported for vectored I/O.
  * Add support for ancillary data.
  * Allow sockets to be split in a read half and a write half.

v0.1.0 - 2020-09-28
  * Initial release.
