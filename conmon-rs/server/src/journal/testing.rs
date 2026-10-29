//! What the tests and the fuzz target of the journal client share.

/// Decode the fields of an entry, the way journald does.
pub fn decode(mut data: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut fields = vec![];
    while !data.is_empty() {
        let end = data.iter().position(|b| *b == b'\n').expect("newline");
        let line = &data[..end];
        if let Some(eq) = line.iter().position(|b| *b == b'=') {
            fields.push((
                String::from_utf8(line[..eq].to_vec()).expect("utf8 key"),
                line[eq + 1..].to_vec(),
            ));
            data = &data[end + 1..];
        } else {
            let rest = &data[end + 1..];
            let len = usize::try_from(u64::from_le_bytes(rest[..8].try_into().expect("length")))
                .expect("length fits");
            assert_eq!(rest[8 + len], b'\n', "value ends with a newline");
            fields.push((
                String::from_utf8(line.to_vec()).expect("utf8 key"),
                rest[8..8 + len].to_vec(),
            ));
            data = &rest[8 + len + 1..];
        }
    }
    fields
}
