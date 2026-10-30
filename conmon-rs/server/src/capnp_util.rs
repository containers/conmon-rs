//! Helpers for the Cap'n Proto requests: the check of their paths and the
//! conversion of their maps.

use crate::conmon_capnp::conmon;
use capnp::struct_list::Reader;
use std::path::Path;

/// Fail for a path of a request which is not absolute, an empty one
/// included. The server would resolve it in its working directory otherwise.
pub(crate) fn check_absolute(what: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{what} {:?} is not absolute", path.display())
    }
    Ok(())
}

/// Convert a capnp map, a list of key value entries, into a rust map, like
/// the environment variables or the labels of a request.
pub(crate) fn into_map<'a, K, V, T>(
    reader: Reader<'a, conmon::text_text_map_entry::Owned>,
) -> capnp::Result<T>
where
    K: From<&'a str>,
    V: From<&'a str>,
    T: FromIterator<(K, V)>,
{
    reader
        .iter()
        .map(|entry| {
            Ok((
                entry.get_key()?.to_str()?.into(),
                entry.get_value()?.to_str()?.into(),
            ))
        })
        .collect()
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
