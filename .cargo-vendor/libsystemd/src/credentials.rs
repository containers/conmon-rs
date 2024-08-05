use crate::errors::{Context, SdError};
use nix::dir;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use std::env;
use std::fs::File;
use std::path::PathBuf;

/// Credential loader for units.
///
/// Credentials are read by systemd on unit startup and exported by their ID.
///
/// **Note**: only the user associated with the unit and the superuser may access credentials.
///
/// More documentation: <https://www.freedesktop.org/software/systemd/man/systemd.exec.html#Credentials>
#[derive(Debug)]
pub struct CredentialsLoader {
    path: PathBuf,
    _dirfd: dir::Dir,
}

impl CredentialsLoader {
    /// Try to open credentials directory.
    pub fn open() -> Result<Self, SdError> {
        let path = Self::path_from_env().ok_or_else(|| {
            SdError::from("No valid environment variable 'CREDENTIALS_DIRECTORY' found")
        })?;

        // NOTE(lucab): we try to open the directory and then store its dirfd, so
        // that we know it exists. We don't further use it now, but in the
        // future we may couple it to something like 'cap-std' helpers.
        let _dirfd = dir::Dir::open(&path, OFlag::O_RDONLY | OFlag::O_DIRECTORY, Mode::empty())
            .with_context(|| format!("Opening credentials directory at '{}'", path.display()))?;

        let loader = Self { path, _dirfd };
        Ok(loader)
    }

    /// Return the location of the credentials directory, if any.
    pub fn path_from_env() -> Option<PathBuf> {
        env::var("CREDENTIALS_DIRECTORY").map(|v| v.into()).ok()
    }

    /// Get credential by ID.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use libsystemd::credentials::CredentialsLoader;
    ///
    /// let loader = CredentialsLoader::open()?;
    /// let token = loader.get("token")?;
    /// let token_metadata = token.metadata()?;
    /// println!("token size: {}", token_metadata.len());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn get(&self, id: impl AsRef<str>) -> Result<File, SdError> {
        let cred_path = self.cred_absolute_path(id.as_ref())?;
        File::open(&cred_path).map_err(|e| {
            let msg = format!("Opening credential at {}: {}", cred_path.display(), e);
            SdError::from(msg)
        })
    }

    /// Validate credential ID and return its absolute path.
    fn cred_absolute_path(&self, id: &str) -> Result<PathBuf, SdError> {
        if id.contains('/') {
            return Err(SdError::from("Invalid credential ID"));
        }

        let abs_path = self.path.join(id);
        Ok(abs_path)
    }

    /// Return an iterator over all existing credentials.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use libsystemd::credentials::CredentialsLoader;
    ///
    /// let loader = CredentialsLoader::open()?;
    /// for entry in loader.iter()? {
    ///   let credential = entry?;
    ///   println!("Credential ID: {}", credential.file_name().to_string_lossy());
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    pub fn iter(&self) -> Result<std::fs::ReadDir, SdError> {
        std::fs::read_dir(&self.path)
            .with_context(|| format!("Opening credential directory at {}", self.path.display()))
    }
}
