use super::*;
use crate::container_log::{
    LineBuffer,
    test_helpers::{json_logger, read},
};
use tempfile::NamedTempFile;

/// Write a chunk the way `ContainerLog` does.
async fn write(logger: &mut JsonLogger, pipe: Pipe, bytes: &[u8]) -> Result<()> {
    let mut buffer = LineBuffer::default();
    let mut lines = buffer.push(pipe, bytes);
    lines.extend(buffer.finish(pipe));
    logger.write_lines(pipe, &lines).await
}

#[tokio::test]
async fn test_json_logger_new() -> Result<()> {
    let file = NamedTempFile::new()?;
    let logger = JsonLogger::new(file.path(), Some(1000));
    assert_eq!(logger.file.path, file.path());
    assert_eq!(logger.file.max_size, Some(1000));
    Ok(())
}

#[tokio::test]
async fn test_json_logger_init() -> Result<()> {
    let (file, _logger) = json_logger(Some(1000)).await?;
    assert!(read(&file)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_json_logger_write() -> Result<()> {
    let (file, mut logger) = json_logger(Some(1000)).await?;

    write(&mut logger, Pipe::StdOut, b"Test log message\nsecond").await?;

    let contents = read(&file)?;
    let mut lines = contents.lines();
    let first = lines.next().context("no first line")?;
    assert!(first.contains(r#""pipe":"stdout""#));
    assert!(first.contains(r#""message":"Test log message""#));
    let second = lines.next().context("no second line")?;
    assert!(second.contains(r#""message":"second""#));
    assert!(lines.next().is_none());
    Ok(())
}

#[tokio::test]
async fn test_json_logger_line_endings() -> Result<()> {
    let (file, mut logger) = json_logger(None).await?;

    write(
        &mut logger,
        Pipe::StdErr,
        b"terminal\r\n  indented \nno newline",
    )
    .await?;

    let contents = read(&file)?;
    let mut lines = contents.lines();
    assert!(
        lines
            .next()
            .context("no line")?
            .contains(r#""message":"terminal"}"#)
    );
    assert!(
        lines
            .next()
            .context("no line")?
            .contains(r#""message":"  indented "}"#)
    );
    assert!(
        lines
            .next()
            .context("no line")?
            .contains(r#""message":"no newline","partial":true}"#)
    );
    assert!(lines.next().is_none());
    Ok(())
}

#[tokio::test]
async fn test_json_logger_reopen() -> Result<()> {
    let (file, mut logger) = json_logger(Some(1000)).await?;

    write(
        &mut logger,
        Pipe::StdOut,
        b"Test log message before reopen\n",
    )
    .await?;
    logger.reopen().await?;
    write(
        &mut logger,
        Pipe::StdOut,
        b"Test log message after reopen\n",
    )
    .await?;

    let contents = read(&file)?;
    assert!(contents.contains("Test log message after reopen"));
    assert!(!contents.contains("Test log message before reopen"));
    Ok(())
}

#[tokio::test]
async fn test_json_logger_rotation_counts_trigger() -> Result<()> {
    // Every entry is about 60 bytes, so the third one triggers a rotation.
    let (file, mut logger) = json_logger(Some(150)).await?;

    write(&mut logger, Pipe::StdOut, b"a\nb\nc\n").await?;

    let contents = read(&file)?;
    assert_eq!(logger.file.bytes_written, contents.len());
    assert_eq!(contents.lines().count(), 1);
    assert!(contents.contains(r#""message":"c""#));
    Ok(())
}

#[tokio::test]
async fn json_log_is_private() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log.json");
    JsonLogger::new(&path, None).init().await?;
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}
