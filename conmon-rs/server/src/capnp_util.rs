pub(crate) use map_entry::into_map;

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
