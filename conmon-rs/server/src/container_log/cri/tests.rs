use super::*;
use crate::container_log::testing::{TIMESTAMP, cri_logger, read};
use anyhow::Context;
use tempfile::NamedTempFile;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// A CRI log entry with the frozen timestamp.
fn entry(pipe: &str, tag: &str, line: &str) -> String {
    format!("{TIMESTAMP} {pipe} {tag} {line}\n")
}

#[tokio::test]
async fn write_stdout_success() -> Result<()> {
    let (file, mut sut) = cri_logger(None).await?;

    sut.write_chunk(Pipe::StdOut, b"this is a line\nand another line\n")
        .await?;

    assert_eq!(
        read(&file)?,
        entry("stdout", "F", "this is a line") + &entry("stdout", "F", "and another line")
    );
    Ok(())
}

#[tokio::test]
async fn write_partial_line() -> Result<()> {
    let (file, mut sut) = cri_logger(None).await?;

    sut.write_chunk(Pipe::StdOut, b"full\npartial").await?;

    assert_eq!(
        read(&file)?,
        entry("stdout", "F", "full") + &entry("stdout", "P", "partial")
    );
    Ok(())
}

#[tokio::test]
async fn write_stdout_stderr_success() -> Result<()> {
    let (file, mut sut) = cri_logger(None).await?;

    sut.write_chunk(Pipe::StdOut, b"a\nb\n").await?;
    sut.write_chunk(Pipe::StdErr, b"c\n").await?;

    assert_eq!(
        read(&file)?,
        entry("stdout", "F", "a") + &entry("stdout", "F", "b") + &entry("stderr", "F", "c")
    );
    Ok(())
}

#[tokio::test]
async fn rotate_on_exceeded_size() -> Result<()> {
    // Room for exactly three entries, the fourth one starts a new file.
    let max_size = 3 * entry("stdout", "F", "a").len();
    let (file, mut sut) = cri_logger(Some(max_size)).await?;

    sut.write_chunk(Pipe::StdOut, b"a\nb\nc\nd\ne\nf\n").await?;

    assert_eq!(
        read(&file)?,
        entry("stdout", "F", "d") + &entry("stdout", "F", "e") + &entry("stdout", "F", "f")
    );
    Ok(())
}

#[tokio::test]
async fn rotate_on_exceeded_size_across_chunks() -> Result<()> {
    // The first chunk fills the file exactly, the first entry of the
    // second one starts a new file.
    let max_size = 3 * entry("stdout", "F", "abcd").len();
    let (file, mut sut) = cri_logger(Some(max_size)).await?;

    sut.write_chunk(Pipe::StdOut, b"abcd\nabcd\nabcd\n").await?;
    assert_eq!(read(&file)?.len(), max_size);
    sut.write_chunk(Pipe::StdErr, b"a\nb\nc\n").await?;

    assert_eq!(
        read(&file)?,
        entry("stderr", "F", "a") + &entry("stderr", "F", "b") + &entry("stderr", "F", "c")
    );
    Ok(())
}

#[tokio::test]
async fn failed_rotation_keeps_the_rest_of_the_chunk() -> Result<()> {
    // The second entry exceeds the size, the third one still fits.
    let max_size = entry("stdout", "F", "abcd").len() + entry("stdout", "F", "a").len();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = CriLogger::new(&path, Some(max_size));
    sut.freeze_timestamp(TIMESTAMP);
    sut.file().open().await?;

    // Keep the open file reachable, then make the rotation fail by putting
    // a directory in its place.
    let old = dir.path().join("old");
    std::fs::hard_link(&path, &old)?;
    std::fs::remove_file(&path)?;
    std::fs::create_dir(&path)?;

    let err = sut
        .write_chunk(Pipe::StdOut, b"abcd\nabcd\na\n")
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.starts_with(&format!(
            "reopen log file because of exceeded size: rename log file to '{}': ",
            path.display()
        )),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&old)?,
        entry("stdout", "F", "abcd") + &entry("stdout", "F", "a")
    );
    Ok(())
}

#[tokio::test]
async fn timestamp_is_rfc3339() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = CriLogger::new(file.path(), None);
    sut.file().open().await?;

    sut.write_chunk(Pipe::StdOut, b"line\n").await?;

    let res = read(&file)?;
    let timestamp = res.split_whitespace().next().context("no timestamp")?;
    OffsetDateTime::parse(timestamp, &Rfc3339).context("unable to parse timestamp")?;
    Ok(())
}

#[tokio::test]
async fn timestamps_are_taken_per_chunk() -> Result<()> {
    let mut sut = Timestamp::default();
    let first = OffsetDateTime::parse(sut.update()?, &Rfc3339)?;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let second = OffsetDateTime::parse(sut.update()?, &Rfc3339)?;
    // Not later, the clock may get set back meanwhile.
    assert_ne!(first, second);
    // Nine digits of the fraction like conmon.
    let fraction = sut.update()?[Timestamp::DATE_TIME_LEN..]
        .strip_prefix('.')
        .context("fraction")?
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    assert_eq!(fraction, 9);
    assert!((OffsetDateTime::now_utc() - second).whole_seconds() < 5);
    Ok(())
}

#[test]
fn timestamps_get_formatted_per_second() -> Result<()> {
    let mut sut = Timestamp::default();
    let parse = |timestamp: &str| OffsetDateTime::parse(timestamp, &Rfc3339);

    // The last nanosecond of a second, then the first one of the next,
    // within the same minute.
    let first = OffsetDateTime::from_unix_timestamp_nanos(1_792_497_601_999_999_999)?;
    assert_eq!(parse(sut.update_at(first)?)?, first);
    let next = first + time::Duration::nanoseconds(2);
    assert_eq!(parse(sut.update_at(next)?)?, next);

    // Within the second only the fraction gets formatted, the date, time of
    // day and offset are the cached ones.
    sut.date_time = "2000-01-01T00:00:00".into();
    let later = next + time::Duration::nanoseconds(5);
    let expected = format!("2000-01-01T00:00:00.000000006{}", sut.offset);
    assert_eq!(sut.update_at(later)?, expected);
    Ok(())
}

#[tokio::test]
async fn init_failure() -> Result<()> {
    let mut sut = CriLogger::new("/file/does/not/exist", None);
    assert!(sut.file().open().await.is_err());
    Ok(())
}

#[tokio::test]
async fn cri_log_is_readable_by_the_group() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // The server runs with the umask 022, the tests with the one of the
    // caller, which may take more.
    let umask = std::fs::read_to_string("/proc/self/status")?
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .context("no umask")?
        .trim()
        .to_owned();
    let expected = 0o640 & !u32::from_str_radix(&umask, 8)?;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mode = || -> Result<u32> { Ok(std::fs::metadata(&path)?.permissions().mode() & 0o777) };
    let mut sut = CriLogger::new(&path, Some(entry("stdout", "F", "a").len()));
    sut.freeze_timestamp(TIMESTAMP);
    sut.file().open().await?;
    assert_eq!(mode()?, expected);

    // Also the file which replaces it on rotation.
    sut.write_chunk(Pipe::StdOut, b"a\nb\n").await?;
    assert_eq!(std::fs::read_to_string(&path)?, entry("stdout", "F", "b"));
    assert_eq!(mode()?, expected);
    Ok(())
}
