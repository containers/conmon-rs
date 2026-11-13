//! File system helpers.

use crate::random_id::RandomId;
use std::{
    env,
    fs::{self, Metadata},
    io::{self, ErrorKind},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use tracing::debug;

/// Remove a file, one which does not exist is no error.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// A file path which gets removed on drop.
#[derive(Debug)]
pub(crate) struct TempFile(PathBuf);

impl TempFile {
    /// A unique path in the `directory`, or in the one for temporary files
    /// without one, whose file does not get created but removed on drop.
    /// This covers early returns and cancelled futures, which would leave
    /// for example the PID file of an exec behind.
    pub(crate) fn new(directory: Option<&Path>, prefix: &str, suffix: &str) -> Self {
        let dir = directory.map_or_else(env::temp_dir, Path::to_path_buf);
        let unique = RandomId::new();
        Self(dir.join(format!("{prefix}{unique}{suffix}")))
    }

    /// The path of the file.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // A plain unlink does not need the runtime and cannot panic without one.
        // The file may be gone already: the terminal listener removes its
        // socket once it got the descriptor, and a runtime may never write a
        // pidfile.
        if let Err(e) = remove_file(&self.0) {
            debug!("Unable to remove temp file {}: {}", self.0.display(), e);
        }
    }
}

/// The identity of a file, which tells whether a path still refers to it.
/// Its inode number cannot be reused while the file is open, like a socket
/// the server still listens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    /// The identity of the file of `metadata`.
    pub(crate) fn new(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    /// Whether the path still refers to the file, without following a
    /// symlink.
    pub(crate) fn is_at(self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|m| Self::new(&m) == self)
    }

    /// Remove the path if it still refers to the file. Another file at the
    /// path stays, a missing one counts as removed.
    pub(crate) fn remove_at(self, path: &Path) -> Result<(), RemoveError> {
        match fs::symlink_metadata(path) {
            Ok(m) if Self::new(&m) == self => fs::remove_file(path).map_err(RemoveError::Remove),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RemoveError::Stat(e)),
        }
    }
}

/// Why [`FileId::remove_at`] failed.
#[derive(Debug)]
pub(crate) enum RemoveError {
    /// The path could not be checked.
    Stat(io::Error),

    /// The file could not be removed.
    Remove(io::Error),
}

/// Fail for a path of a request which is not absolute, an empty one
/// included. The server would resolve it in its working directory otherwise.
pub(crate) fn check_absolute(what: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{what} {:?} is not absolute", path.display())
    }
    Ok(())
}

/// Fail for a path of a request which cannot be a file the server writes:
/// one which is not absolute, see [`check_absolute`], or one which does not
/// name a file, like `/` or one which ends in `..`, `/` or `/.`. Writing it
/// would fail anyway, but only once the container exited, when nobody gets
/// told anymore. [`Path::file_name`] alone would take the last two for the
/// file before them.
pub(crate) fn check_file_path(what: &str, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    check_absolute(what, path)?;
    let bytes = path.as_os_str().as_bytes();
    if path.file_name().is_none() || bytes.ends_with(b"/") || bytes.ends_with(b"/.") {
        anyhow::bail!("{what} {:?} does not name a file", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_count_as_removed() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file");
        remove_file(&path)?;
        fs::write(&path, "")?;
        remove_file(&path)?;
        assert!(!path.exists());
        assert!(remove_file(dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn temp_files_get_removed_on_drop() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = TempFile::new(Some(dir.path()), "prefix-", ".suffix");
        let path = file.path().to_path_buf();
        let name = path.file_name().and_then(|name| name.to_str());
        assert!(name.is_some_and(|n| n.starts_with("prefix-") && n.ends_with(".suffix")));
        assert_eq!(path.parent(), Some(dir.path()));
        assert_ne!(
            TempFile::new(Some(dir.path()), "prefix-", ".suffix").path(),
            path
        );

        // Only the path, the file is up to the user.
        assert!(!path.exists());
        fs::write(&path, "")?;
        drop(file);
        assert!(!path.exists());

        // One which never got created, in the directory for temporary files.
        let file = TempFile::new(None, "prefix-", ".suffix");
        assert_eq!(file.path().parent(), Some(env::temp_dir().as_path()));
        drop(file);
        Ok(())
    }

    #[test]
    fn only_the_same_file_gets_removed() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, "")?;
        let id = FileId::new(&fs::metadata(&path)?);
        assert!(id.is_at(&path));
        // A symlink to the file is not the file.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link)?;
        assert!(!id.is_at(&link));

        // Replaced by another file, which stays.
        let other = dir.path().join("other");
        fs::write(&other, "")?;
        let _held = fs::File::open(&path)?;
        fs::rename(&other, &path)?;
        assert!(!id.is_at(&path));
        id.remove_at(&path).expect("another file stays");
        assert!(path.exists());

        let id = FileId::new(&fs::metadata(&path)?);
        id.remove_at(&path).expect("remove the file");
        assert!(!path.exists());
        id.remove_at(&path)
            .expect("a missing file counts as removed");

        // Only the file itself gets stat failures reported.
        let under_file = path.join("file");
        fs::write(&path, "")?;
        let id = FileId::new(&fs::metadata(&path)?);
        assert!(matches!(
            id.remove_at(&under_file),
            Err(RemoveError::Stat(_))
        ));
        Ok(())
    }

    #[test]
    fn paths_have_to_be_absolute() {
        assert!(check_absolute("bundle path", Path::new("/bundle")).is_ok());
        for path in ["", "bundle", "./bundle"] {
            let err = check_absolute("bundle path", Path::new(path)).unwrap_err();
            assert!(err.to_string().contains("is not absolute"), "{err}");
        }
    }

    #[test]
    fn file_paths_have_to_name_a_file() {
        for path in [
            "/exit",
            "/dir/exit",
            "/dir/./exit",
            "/dir/.exit",
            "/dir/exit.",
        ] {
            assert!(check_file_path("exit path", Path::new(path)).is_ok());
        }
        // The last two would fail the rename onto them at the exit.
        for path in ["/", "/dir/..", "/..", "/dir/exit/", "/dir/exit/."] {
            let err = check_file_path("exit path", Path::new(path)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("exit path {path:?} does not name a file")
            );
        }
        let err = check_file_path("exit path", Path::new("exit")).unwrap_err();
        assert!(err.to_string().contains("is not absolute"), "{err}");
    }
}
