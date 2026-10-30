//! Helpers for the Cap'n Proto requests: the check of their paths and the
//! conversion of their maps.

pub(crate) use map_entry::into_map;

use std::path::Path;

/// Fail for a path of a request which is not absolute, an empty one
/// included. The server would resolve it in its working directory otherwise.
pub(crate) fn check_absolute(what: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{what} {:?} is not absolute", path.display())
    }
    Ok(())
}

/// Helper functions to convert between capnp maps (List's of MapEntries) and rust maps
mod map_entry {
    use crate::conmon_capnp::conmon;
    use capnp::{struct_list::Reader, traits::OwnedStruct};

    /// Convert from a MapEntry capnp struct to a (key, value) tuple.
    pub(crate) trait IntoKeyValue<'a, K, V>: OwnedStruct {
        fn into_key_value(entry: Self::Reader<'a>) -> capnp::Result<(K, V)>;
    }

    impl<'a, K, V> IntoKeyValue<'a, K, V> for conmon::text_text_map_entry::Owned
    where
        K: From<&'a str>,
        V: From<&'a str>,
    {
        fn into_key_value(entry: Self::Reader<'a>) -> capnp::Result<(K, V)> {
            Ok((
                entry.get_key()?.to_str()?.into(),
                entry.get_value()?.to_str()?.into(),
            ))
        }
    }

    /// Convert a capnp map reader (`struct_list::Reader`) into a rust map (`impl FromIterator<(K, V)>`).
    pub(crate) fn into_map<'a, K, V, E, T>(reader: Reader<'a, E>) -> capnp::Result<T>
    where
        E: IntoKeyValue<'a, K, V>,
        T: FromIterator<(K, V)>,
    {
        reader.into_iter().map(E::into_key_value).collect()
    }
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
