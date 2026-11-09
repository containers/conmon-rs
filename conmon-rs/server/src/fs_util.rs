//! File system helpers.

use std::{
    fs::{self, Metadata},
    io::{self, ErrorKind},
    os::unix::fs::MetadataExt,
    path::Path,
};

/// Remove a file, one which does not exist is no error.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
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
    fn only_the_same_file_gets_removed() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, "")?;
        let id = FileId::new(&fs::metadata(&path)?);
        assert!(id.is_at(&path));

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
}
