use super::*;
use crate::container_log::test_helpers::{TIMESTAMP, cri_logger, read};
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
async fn timestamp_is_rfc3339() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = CriLogger::new(file.path(), None);
    sut.init().await?;

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

#[tokio::test]
async fn init_failure() -> Result<()> {
    let mut sut = CriLogger::new("/file/does/not/exist", None);
    assert!(sut.init().await.is_err());
    Ok(())
}
