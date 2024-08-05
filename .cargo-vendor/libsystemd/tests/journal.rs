use std::process::Command;

use libsystemd::logging::Priority;
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::collections::HashMap;
use std::time::Duration;

fn random_name(prefix: &str) -> String {
    format!(
        "{}_{}",
        prefix,
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect::<String>()
    )
}

/// Retry `f` 10 times 100ms apart.
///
/// When `f` returns an error wait 100ms and try it again, up to ten times.
/// If the last attempt failed return the error returned by that attempt.
///
/// If `f` returns Ok immediately return the result.
fn retry<T, E>(f: impl Fn() -> Result<T, E>) -> Result<T, E> {
    let attempts = 10;
    let interval = Duration::from_millis(100);
    for attempt in (0..attempts).rev() {
        match f() {
            Ok(result) => return Ok(result),
            Err(e) if attempt == 0 => return Err(e),
            Err(_) => std::thread::sleep(interval),
        }
    }
    unreachable!()
}

/// Read from journal with `journalctl`.
///
/// `test_name` is the randomized name of the test being run, and gets
/// added as `TEST_NAME` match to the `journalctl` call, to make sure to
/// only select journal entries originating from and relevant to the
/// current test.
fn read_from_journal(test_name: &str) -> Vec<HashMap<String, String>> {
    let stdout = String::from_utf8(
        Command::new("journalctl")
            .args(&["--user", "--output=json"])
            // Filter by the PID of the current test process
            .arg(format!("_PID={}", std::process::id()))
            .arg(format!("TEST_NAME={}", test_name))
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();

    stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Read exactly one line from journal for the given test name.
///
/// Try to read lines for `testname` from journal, and `retry()` if the wasn't
/// _exactly_ one matching line.
fn retry_read_one_line_from_journal(testname: &str) -> HashMap<String, String> {
    retry(|| {
        let mut messages = read_from_journal(testname);
        if messages.len() == 1 {
            Ok(messages.pop().unwrap())
        } else {
            Err(format!(
                "one messages expected, got {} messages",
                messages.len()
            ))
        }
    })
    .unwrap()
}

#[test]
fn simple_message() {
    let test_name = random_name("simple_message");
    libsystemd::logging::journal_send(
        Priority::Info,
        "Hello World",
        vec![
            ("TEST_NAME", test_name.as_str()),
            ("FOO", "another piece of data"),
        ]
        .into_iter(),
    )
    .unwrap();

    let message = retry_read_one_line_from_journal(&test_name);
    assert_eq!(message["MESSAGE"], "Hello World");
    assert_eq!(message["TEST_NAME"], test_name);
    assert_eq!(message["PRIORITY"], "6");
    assert_eq!(message["FOO"], "another piece of data");
}

#[test]
fn multiline_message() {
    let test_name = random_name("multiline_message");
    libsystemd::logging::journal_send(
        Priority::Info,
        "Hello\nMultiline\nWorld",
        vec![("TEST_NAME", test_name.as_str())].into_iter(),
    )
    .unwrap();

    let message = retry_read_one_line_from_journal(&test_name);
    assert_eq!(message["MESSAGE"], "Hello\nMultiline\nWorld");
    assert_eq!(message["TEST_NAME"], test_name);
    assert_eq!(message["PRIORITY"], "6");
}

#[test]
fn multiline_message_trailing_newline() {
    let test_name = random_name("multiline_message_trailing_newline");
    libsystemd::logging::journal_send(
        Priority::Info,
        "A trailing newline\n",
        vec![("TEST_NAME", test_name.as_str())].into_iter(),
    )
    .unwrap();

    let message = retry_read_one_line_from_journal(&test_name);
    assert_eq!(message["MESSAGE"], "A trailing newline\n");
    assert_eq!(message["TEST_NAME"], test_name);
    assert_eq!(message["PRIORITY"], "6");
}
