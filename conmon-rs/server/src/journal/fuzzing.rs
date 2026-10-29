use super::{Entry, Priority, testing::decode};

/// Encode the fields the input describes into an entry and decode it with
/// the decoder of the tests, which has to give back exactly the same
/// fields. The first byte picks the priority and where the fields get
/// split into a second entry, which the first one gets extended with. Each
/// field is a byte for the length of its key, which gets built from the
/// next bytes as a valid name, a byte for the length of its value and the
/// value. The first field is the message, its key gets consumed but not
/// used. A key cut off by the end of the input ends the fields, a value may
/// be shorter.
pub fn entry(input: &[u8]) {
    let Some((&first, mut input)) = input.split_first() else {
        return;
    };
    let mut fields = vec![];
    let mut take = |len: usize| {
        let (taken, rest) = input.split_at(len.min(input.len()));
        input = rest;
        taken
    };
    let mut message = None;
    while let [key_len] = *take(1) {
        let key_len = usize::from(key_len % 64) + 1;
        let key = take(key_len);
        if key.len() < key_len {
            break;
        }
        let key: String = key
            .iter()
            .enumerate()
            .map(|(i, b)| {
                // Not starting with a digit or an underscore.
                let chars: &[u8] = if i == 0 {
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"
                } else {
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"
                };
                char::from(chars[usize::from(*b) % chars.len()])
            })
            .collect();
        assert!(Entry::valid_key(&key), "{key}");
        let value_len = take(1).first().copied().unwrap_or_default();
        let value = take(usize::from(value_len)).to_vec();
        if message.is_none() {
            message = Some(value);
        } else {
            fields.push((key, value));
        }
    }
    let message = message.unwrap_or_default();
    let priority = match first % 5 {
        0 => Priority::Error,
        1 => Priority::Warning,
        2 => Priority::Notice,
        3 => Priority::Info,
        _ => Priority::Debug,
    };

    // Reused like the loggers do.
    let mut entry = Entry::new(Priority::Debug, "previous");
    entry.field("PREVIOUS", "entry").reset(priority, &message);
    let split = usize::from(first / 5).min(fields.len());
    let (own, extended) = fields.split_at(split);
    for (key, value) in own {
        entry.field(key, value);
    }
    let mut extension = Entry::default();
    for (key, value) in extended {
        extension.field(key, value);
    }
    entry.extend(&extension);

    let mut expected = vec![
        ("PRIORITY".to_string(), vec![b'0' + priority as u8]),
        ("MESSAGE".to_string(), message),
    ];
    expected.extend(fields);
    assert_eq!(decode(entry.as_bytes()), expected);
}
