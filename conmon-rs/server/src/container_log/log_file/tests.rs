use super::*;
use std::fs;
use tempfile::NamedTempFile;

#[tokio::test]
async fn reopen_resets_size() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = LogFile::new(file.path(), None, Some(10));
    sut.open().await?;

    sut.buf.extend_from_slice(b"12345678\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    sut.reopen().await?;

    // The reopened file is empty, so the entry fits without a rotation.
    sut.buf.extend_from_slice(b"abc\n");
    sut.commit_entry(0).await?;
    sut.buf.extend_from_slice(b"def\n");
    sut.commit_entry(4).await?;
    sut.flush().await?;

    assert_eq!(fs::read_to_string(file.path())?, "abc\ndef\n");
    assert_eq!(sut.bytes_written, 8);
    Ok(())
}

#[tokio::test]
async fn reopen_after_failed_flush() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = LogFile::new(&path, None, None);
    sut.open().await?;
    // A file which cannot be written fails the flush.
    sut.file = Some(Arc::new(std::fs::File::open(&path)?));
    sut.buf.extend_from_slice(b"lost\n");

    sut.reopen().await?;
    assert!(sut.buf.is_empty());
    sut.buf.extend_from_slice(b"new\n");
    sut.flush().await?;
    assert_eq!(fs::read_to_string(&path)?, "new\n");
    Ok(())
}

#[tokio::test]
async fn rotation_writes_earlier_entries_of_chunk_to_old_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = LogFile::new(&path, None, Some(10));
    sut.open().await?;
    // A reader which follows the old file.
    let old = dir.path().join("old");
    fs::hard_link(&path, &old)?;

    sut.buf.extend_from_slice(b"first\n");
    sut.commit_entry(0).await?;
    sut.buf.extend_from_slice(b"second\n");
    sut.commit_entry(6).await?;
    assert_eq!(sut.buf, b"second\n");
    sut.flush().await?;

    assert_eq!(fs::read_to_string(&old)?, "first\n");
    assert_eq!(fs::read_to_string(&path)?, "second\n");
    assert_eq!(sut.bytes_written, 7);
    Ok(())
}

#[tokio::test]
async fn failed_rotation_writes_old_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = LogFile::new(&path, None, Some(10));
    sut.open().await?;

    // Keep the open file reachable, then make the rotation fail by putting
    // a directory in its place.
    let old = dir.path().join("old");
    fs::hard_link(&path, &old)?;
    fs::remove_file(&path)?;
    fs::create_dir(&path)?;

    sut.buf.extend_from_slice(b"first\n");
    sut.commit_entry(0).await?;
    sut.buf.extend_from_slice(b"second\n");
    let err = sut
        .commit_entry(6)
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

    assert!(sut.buf.is_empty());
    assert_eq!(sut.bytes_written, 6);
    assert_eq!(fs::read_to_string(&old)?, "first\n");
    assert!(!dir.path().join(".log.conmonrs-tmp").exists());

    // The rest of the chunk still gets written, as long as it fits. The
    // rotation is not tried again before the next chunk, which would work
    // now, the entries which exceed the size get dropped.
    fs::remove_dir(&path)?;
    sut.buf.extend_from_slice(b"ab\n");
    sut.commit_entry(0).await?;
    sut.buf.extend_from_slice(b"third\n");
    sut.commit_entry(3).await?;
    sut.end_chunk().await?;
    assert_eq!(fs::read_to_string(&old)?, "first\nab\n");
    assert!(!path.exists());

    sut.buf.extend_from_slice(b"fourth\n");
    sut.commit_entry(0).await?;
    sut.end_chunk().await?;
    assert_eq!(fs::read_to_string(&old)?, "first\nab\n");
    assert_eq!(fs::read_to_string(&path)?, "fourth\n");
    Ok(())
}

#[tokio::test]
async fn rotation_replaces_the_file() -> Result<()> {
    use std::{io::Read, os::unix::fs::MetadataExt};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = LogFile::new(&path, Some(0o600), Some(10));
    sut.open().await?;
    sut.buf.extend_from_slice(b"first\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;

    // A reader following the log, like the kubelet.
    let mut reader = File::open(&path)?;
    let mut old = String::new();
    reader.read_to_string(&mut old)?;
    assert_eq!(old, "first\n");
    let ino = fs::metadata(&path)?.ino();

    sut.buf.extend_from_slice(b"second\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    let metadata = fs::metadata(&path)?;
    assert_ne!(metadata.ino(), ino);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(fs::read_to_string(&path)?, "second\n");
    assert!(!dir.path().join(".log.conmonrs-tmp").exists());

    // The old file keeps its content, the reader is at its end.
    assert_eq!(reader.metadata()?.ino(), ino);
    assert_eq!(reader.read_to_string(&mut old)?, 0);
    assert_eq!(reader.metadata()?.len(), 6);

    // The reopen after the kubelet renamed the file starts a new one.
    let rotated = dir.path().join("log.1");
    fs::rename(&path, &rotated)?;
    sut.reopen().await?;
    sut.buf.extend_from_slice(b"third\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    assert_eq!(fs::read_to_string(&rotated)?, "second\n");
    assert_eq!(fs::read_to_string(&path)?, "third\n");
    Ok(())
}

#[tokio::test]
async fn writes_append_after_truncation() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let mut sut = LogFile::new(&path, None, Some(20));
    sut.open().await?;
    sut.buf.extend_from_slice(b"first entry 12345\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    let ino = fs::metadata(&path)?.ino();

    // Like `> 0.log` of an admin, the file shrinks below the offset. The
    // next entry continues at its end and does not rotate it, the file
    // holds far less than the size.
    File::options().write(true).open(&path)?.set_len(0)?;
    sut.buf.extend_from_slice(b"second\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    assert_eq!(fs::read_to_string(&path)?, "second\n");
    assert_eq!(fs::metadata(&path)?.ino(), ino);
    assert_eq!(sut.bytes_written, 7);

    // The size still applies to what the file holds.
    sut.buf.extend_from_slice(b"third entry 1234\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    assert_eq!(fs::read_to_string(&path)?, "third entry 1234\n");
    assert_ne!(fs::metadata(&path)?.ino(), ino);
    Ok(())
}

#[tokio::test]
async fn open_removes_a_stale_tmp_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    let tmp = LogFile::tmp_path(&path);
    fs::write(&tmp, "left behind")?;
    LogFile::new(&path, None, None).open().await?;
    assert!(!tmp.exists());
    Ok(())
}

#[tokio::test]
async fn torn_entries_get_ended() -> Result<()> {
    use nix::{fcntl::OFlag, unistd::pipe2};
    use std::io::Read;

    // A pipe which is full fails a write in the middle, like a full disk.
    let (reader, writer) = pipe2(OFlag::O_NONBLOCK)?;
    let mut reader = File::from(reader);
    let mut sut = LogFile::new("unused", None, None);
    sut.file = Some(Arc::new(File::from(writer)));

    let entry = [vec![b'a'; 1024 * 1024], b"\n".to_vec()].concat();
    sut.buf.extend_from_slice(&entry);
    // A large entry gets written right away.
    let err = sut
        .commit_entry(0)
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(err.starts_with("write log chunk: "), "{err}");
    assert!(sut.buf.is_empty());
    assert!(sut.torn);
    let mut written = vec![];
    let _ = reader.read_to_end(&mut written);
    assert!(!written.is_empty() && written.len() < entry.len());
    assert_eq!(sut.bytes_written, written.len());

    // The next entry starts on a line of its own.
    sut.buf.extend_from_slice(b"next\n");
    sut.commit_entry(0).await?;
    sut.flush().await?;
    assert!(!sut.torn);
    let mut next = vec![];
    let _ = reader.read_to_end(&mut next);
    assert_eq!(next, b"\nnext\n");
    assert_eq!(sut.bytes_written, written.len() + next.len());
    Ok(())
}

#[test]
fn tmp_path_is_hidden_from_the_kubelet() {
    assert_eq!(
        LogFile::tmp_path(Path::new("/logs/0.log")),
        Path::new("/logs/.0.log.conmonrs-tmp")
    );
}

#[tokio::test]
async fn failed_flush_drops_chunk() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = LogFile::new(file.path(), None, None);

    // Not opened yet, so the write fails.
    sut.buf.extend_from_slice(b"lost\n");
    sut.commit_entry(0).await?;
    let err = sut.flush().await.err().map(|e| format!("{e:#}"));
    assert_eq!(err.as_deref(), Some(LogFile::ERR_UNINITIALIZED));
    assert!(sut.buf.is_empty());
    assert_eq!(sut.bytes_written, 0);

    sut.open().await?;
    sut.buf.extend_from_slice(b"kept\n");
    sut.flush().await?;
    assert_eq!(fs::read_to_string(file.path())?, "kept\n");
    Ok(())
}

#[tokio::test]
async fn open_does_not_follow_symlinks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let target = dir.path().join("target");
    fs::write(&target, "keep")?;
    let link = dir.path().join("log");
    std::os::unix::fs::symlink(&target, &link)?;

    let mut sut = LogFile::new(&link, Some(0o600), None);
    let err = sut
        .open()
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.starts_with(&format!("open log file path '{}': ", link.display())),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&target)?, "keep");
    Ok(())
}

#[tokio::test]
async fn open_appends_to_the_existing_log() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    fs::write(&path, "old\n")?;
    let mut sut = LogFile::new(&path, None, Some(10));
    sut.open().await?;
    assert_eq!(sut.bytes_written, 4);

    sut.buf.extend_from_slice(b"new\n");
    sut.commit_entry(0).await?;
    sut.end_chunk().await?;
    assert_eq!(fs::read_to_string(&path)?, "old\nnew\n");

    // The existing content counts for the size. A temporary file which got
    // left behind does not end up in the new file.
    fs::write(LogFile::tmp_path(&path), "left behind")?;
    sut.buf.extend_from_slice(b"more\n");
    sut.commit_entry(0).await?;
    sut.end_chunk().await?;
    assert_eq!(fs::read_to_string(&path)?, "more\n");
    Ok(())
}

#[tokio::test]
async fn open_starts_after_a_torn_log_on_a_line_of_its_own() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    fs::write(&path, "cut")?;
    let mut sut = LogFile::new(&path, None, None);
    sut.open().await?;

    sut.buf.extend_from_slice(b"new\n");
    sut.commit_entry(0).await?;
    sut.end_chunk().await?;
    assert_eq!(fs::read_to_string(&path)?, "cut\nnew\n");
    assert_eq!(sut.bytes_written, 8);
    Ok(())
}

#[tokio::test]
async fn buffer_is_held_only_during_a_chunk() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = LogFile::new(file.path(), None, None);
    sut.open().await?;
    assert_eq!(sut.buf.capacity(), 0);

    sut.buf().extend_from_slice(b"line\n");
    sut.flush().await?;
    assert_eq!(fs::read_to_string(file.path())?, "line\n");
    assert!(sut.buf.capacity() >= LogFile::BUF_CAPACITY);

    // A full read chunk of short lines grows it beyond its initial
    // capacity, which an idle container does not keep.
    let lines = vec![b"x\n".as_slice(); 4096];
    let mut grown = 0;
    sut.write_entries(lines, |buf, line| {
        buf.extend_from_slice(&[b' '; 45]);
        buf.extend_from_slice(line);
        grown = grown.max(buf.capacity());
    })
    .await?;
    assert!(grown > LogFile::BUF_CAPACITY, "{grown}");
    assert_eq!(sut.buf.capacity(), 0);
    assert_eq!(fs::metadata(file.path())?.len(), 5 + 4096 * 47);
    Ok(())
}

#[tokio::test]
async fn short_lines_are_written_in_bounded_pieces() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = LogFile::new(file.path(), None, None);
    sut.open().await?;

    let mut expected = String::new();
    for i in 0..10_000 {
        let start = sut.buf.len();
        let entry = format!("{i}\n");
        sut.buf.extend_from_slice(entry.as_bytes());
        expected.push_str(&entry);
        sut.commit_entry(start).await?;
        assert!(sut.buf.len() < LogFile::FLUSH_SIZE);
    }
    sut.end_chunk().await?;
    assert_eq!(sut.buf.capacity(), 0);
    assert_eq!(fs::read_to_string(file.path())?, expected);
    Ok(())
}
