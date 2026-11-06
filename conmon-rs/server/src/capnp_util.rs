//! The check of the paths of the Cap'n Proto requests.

use std::path::Path;

/// Fail for a path of a request which is not absolute, an empty one
/// included. The server would resolve it in its working directory otherwise.
pub(crate) fn check_absolute(what: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{what} {:?} is not absolute", path.display())
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_have_to_be_absolute() {
        assert!(check_absolute("bundle path", Path::new("/bundle")).is_ok());
        for path in ["", "bundle", "./bundle"] {
            let err = check_absolute("bundle path", Path::new(path)).unwrap_err();
            assert!(err.to_string().contains("is not absolute"), "{err}");
        }
    }
}
