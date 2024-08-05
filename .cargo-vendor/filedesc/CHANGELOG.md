v0.6.3:
  * Implement `From<FileDesc>` for `OwnedFd`.

v0.6.2:
  * Implement `From<OwnedFd>` for `FileDesc`.

v0.6.1
  * Fix tests and update example.

v0.6.0
  * Make use of `OwnedFd`, `BorrowedFd` and `AsFd`.

v0.5.0
  * Make `FileDesc::new` unsafe.
  * Make `FileDesc::duplicate` safe again.

v0.4.0
  * Make `FileDesc::duplicate` and `FileDesc::duplicate_from` unsafe (see the docs for more information).
  * Fix safety documentation of `FileDesc::duplicate_raw_fd`.

v0.3.1:
  * Generate a clear compilation error on non-Unix platforms.

v0.3.0:
  * Add `new()` function that convert `IntoRawFd` objects.
  * Add `duplicate_from()` function that duplicates `AsRawFd` objects.
  * Remember if `F_DUPFD_CLOEXEC` is unsupported to avoid unnecessary syscalls.

v0.2.0:
  * Remove `check_ret()` from public API.

v0.1.0:
  * Initial release.
