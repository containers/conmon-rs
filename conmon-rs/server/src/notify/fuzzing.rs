use super::NotifySocket;

/// Filter a notify message of a container. Only the allowed assignments
/// pass, one per line, the result is never longer than the message and
/// filtering it again keeps it as it is. A message gets relayed if it
/// has any of them and is not too long.
pub fn filter(message: &[u8]) {
    let filtered = NotifySocket::filter_message(message);
    assert!(filtered.len() <= message.len());
    if !filtered.is_empty() {
        for line in filtered.split(|b| *b == b'\n') {
            assert!(
                NotifySocket::PASS_LINES.contains(&line)
                    || NotifySocket::PASS_PREFIXES
                        .iter()
                        .any(|p| line.starts_with(p)),
                "{line:?}"
            );
        }
    }
    assert_eq!(NotifySocket::filter_message(&filtered), filtered);

    let relayed = NotifySocket::check_message(message);
    if message.len() > NotifySocket::MAX_MESSAGE_SIZE || filtered.is_empty() {
        assert_eq!(relayed, None);
    } else {
        assert_eq!(relayed, Some(filtered));
    }
}
