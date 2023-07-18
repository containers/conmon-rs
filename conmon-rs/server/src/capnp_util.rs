pub(crate) use map_entry::into_map;

#[allow(unused)]
pub(crate) use map_entry::set_map;

/// Helper functions to convert between capnp maps (List's of MapEntries) and rust maps
mod map_entry {
    use capnp::{
        struct_list::{Builder, Reader},
        traits::OwnedStruct,
    };
    use conmon_common::conmon_capnp::conmon;

    /// Convert from a MapEntry capnp struct to a (key, value) tuple.
    pub(crate) trait IntoKeyValue<'a, K, V>: OwnedStruct {
        fn into_key_value(entry: Self::Reader<'a>) -> capnp::Result<(K, V)>;
    }

    /// Set the key and value properties of a MapEntry capnp struct.
    pub(crate) trait SetKeyValue<K, V>: OwnedStruct {
        fn set_key_value(entry: Self::Builder<'_>, key: K, value: V);
    }

    impl<'a, K, V> IntoKeyValue<'a, K, V> for conmon::text_text_map_entry::Owned
    where
        K: From<&'a str>,
        V: From<&'a str>,
    {
        fn into_key_value(entry: Self::Reader<'a>) -> capnp::Result<(K, V)> {
            Ok((entry.get_key()?.into(), entry.get_value()?.into()))
        }
    }

    impl<K, V> SetKeyValue<K, V> for conmon::text_text_map_entry::Owned
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        fn set_key_value(mut entry: Self::Builder<'_>, key: K, value: V) {
            entry.set_key(key.as_ref());
            entry.set_value(value.as_ref());
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

    /// Set a capnp map property from a rust map (`impl IntoIterator<Item=(K, V)>`).
    ///
    /// The `init` function is used to allocate a `struct_list::Builder`.
    ///
    /// ## Example
    /// Set the `exampleMap` property in the response capnp builder to the rust map `data`
    /// ```ignore
    /// set_from_map(data, |size| response.init_example_map(size));
    /// ```
    pub(crate) fn set_map<'a, K, V, E, T>(data: T, init: impl FnOnce(u32) -> Builder<'a, E>)
    where
        T: IntoIterator<Item = (K, V)>,
        T::IntoIter: ExactSizeIterator,
        E: SetKeyValue<K, V>,
    {
        let data = data.into_iter();
        let size = if let Ok(size) = data.len().try_into() {
            size
        } else {
            panic!("map with more then u32::MAX entries")
        };
        let mut list = init(size);
        for (i, (key, value)) in data.enumerate() {
            let entry = list.reborrow().get(i as u32);
            E::set_key_value(entry, key, value);
        }
    }
}
