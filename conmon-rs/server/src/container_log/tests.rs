use super::*;
use std::fs;
use tempfile::NamedTempFile;

fn collect(lines: Vec<Line>) -> Vec<(String, bool)> {
    lines
        .into_iter()
        .map(|line| {
            (
                String::from_utf8_lossy(&line.text).into_owned(),
                line.partial,
            )
        })
        .collect()
}

#[test]
fn line_buffer_joins_chunks_per_pipe() {
    let mut buffer = LineBuffer::default();
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"first\nab")),
        vec![("first".into(), false)]
    );
    assert!(buffer.has_pending(Pipe::StdOut));
    // The other pipe has its own tail.
    assert!(!buffer.has_pending(Pipe::StdErr));
    assert_eq!(
        collect(buffer.push(Pipe::StdErr, b"err\n")),
        vec![("err".into(), false)]
    );
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"cd\nthird")),
        vec![("abcd".into(), false)]
    );

    // The output ended, the rest is a partial line.
    assert_eq!(
        collect(buffer.finish(Pipe::StdOut)),
        vec![("third".into(), true)]
    );
    assert!(buffer.finish(Pipe::StdOut).is_empty());
    assert!(!buffer.has_pending(Pipe::StdOut));
}

#[test]
fn line_buffer_flushes_paused_output() {
    let mut buffer = LineBuffer::default();
    assert!(buffer.push(Pipe::StdOut, b"prompt> ").is_empty());
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("prompt> ".into(), true)]
    );
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    // The continuation is its own line.
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"ls\n")),
        vec![("ls".into(), false)]
    );

    // A pause inside a character or before a "\n" keeps them for the
    // next chunk.
    let euro = "€".as_bytes();
    assert!(
        buffer
            .push(Pipe::StdOut, &[b"a", &euro[..1]].concat())
            .is_empty()
    );
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("a".into(), true)]
    );
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    assert!(buffer.has_pending(Pipe::StdOut));
    assert!(
        buffer
            .push(Pipe::StdOut, &[&euro[1..], b"\r"].concat())
            .is_empty()
    );
    assert_eq!(
        collect(buffer.flush(Pipe::StdOut)),
        vec![("€".into(), true)]
    );
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("".into(), false)]
    );

    // Once the output ended, everything goes.
    assert!(buffer.push(Pipe::StdOut, &euro[..1]).is_empty());
    assert!(buffer.flush(Pipe::StdOut).is_empty());
    assert_eq!(buffer.finish(Pipe::StdOut).len(), 1);
    assert!(!buffer.has_pending(Pipe::StdOut));
}

#[test]
fn line_buffer_handles_split_line_endings_and_characters() {
    let mut buffer = LineBuffer::default();
    // "\r\n" split between two chunks.
    assert!(buffer.push(Pipe::StdOut, b"terminal\r").is_empty());
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("terminal".into(), false)]
    );

    // A multi byte character split between two chunks.
    let euro = "€".as_bytes();
    assert!(buffer.push(Pipe::StdOut, &euro[..1]).is_empty());
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
        vec![("€".into(), false)]
    );
}

#[test]
fn line_buffer_borrows_whole_lines_of_the_chunk() {
    let mut buffer = LineBuffer::default();
    let chunk = b"first\r\nsecond\nthi";
    let lines = buffer.push(Pipe::StdOut, chunk);
    assert_eq!(
        lines,
        [
            Line {
                text: Text::Chunk(&chunk[..5]),
                partial: false,
            },
            Line {
                text: Text::Chunk(&chunk[7..13]),
                partial: false,
            },
        ]
    );
    assert!(
        lines
            .iter()
            .all(|line| chunk.as_ptr_range().contains(&line.text.as_ptr()))
    );

    // The line which started in the buffer gets completed there, the ones
    // after it borrow the chunk again.
    let chunk = b"rd\nfourth\nfif";
    let lines = buffer.push(Pipe::StdOut, chunk);
    assert!(matches!(&lines[0].text, Text::Buffered(text) if text == "third"));
    assert_eq!(lines[1].text, Text::Chunk(b"fourth"));
    assert!(chunk.as_ptr_range().contains(&lines[1].text.as_ptr()));
    assert_eq!(lines.len(), 2);
    assert_eq!(&buffer.stdout[..], b"fif");

    // Without an ending, the chunk only adds to the buffer.
    assert!(buffer.push(Pipe::StdOut, b"th").is_empty());
    assert_eq!(&buffer.stdout[..], b"fifth");
}

#[test]
fn line_buffer_cuts_long_lines() {
    let euro = "€".as_bytes();
    let long = vec![b'a'; LineBuffer::MAX_LINE - 1];

    // An unfinished character at the cut stays buffered.
    let mut buffer = LineBuffer::default();
    let chunk = [long.as_slice(), &euro[..1]].concat();
    let out = buffer.push(Pipe::StdOut, &chunk);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
    assert!(out[0].partial);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[1..], b"\n"].concat())),
        vec![("€".into(), false)]
    );

    // So does a "\r" which may be followed by its "\n".
    let mut buffer = LineBuffer::default();
    let chunk = [long.as_slice(), b"\r"].concat();
    let out = buffer.push(Pipe::StdOut, &chunk);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 1);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, b"\n")),
        vec![("".into(), false)]
    );

    // An invalid byte in front does not cut an unfinished character.
    let mut buffer = LineBuffer::default();
    let mut line = vec![b'a'; LineBuffer::MAX_LINE - 4];
    line.extend_from_slice(&[0xff, b'a', euro[0], euro[1]]);
    let out = buffer.push(Pipe::StdOut, &line);
    assert_eq!(out[0].text.len(), LineBuffer::MAX_LINE - 2);
    assert_eq!(
        collect(buffer.push(Pipe::StdOut, &[&euro[2..], b"\n"].concat())),
        vec![("€".into(), false)]
    );

    // Invalid bytes are not held back, the buffer would grow forever.
    let mut buffer = LineBuffer::default();
    let mut invalid = vec![0xff; LineBuffer::MAX_LINE];
    invalid.push(0xfe);
    let out = buffer.push(Pipe::StdOut, &invalid);
    assert_eq!(out[0].text.len(), invalid.len());
    assert!(!buffer.has_pending(Pipe::StdOut));
    assert!(buffer.stdout.capacity() <= LineBuffer::MAX_LINE);
}

/// A small deterministic random number generator (xorshift), so that a
/// failing case can be reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("small bound");
        usize::try_from(self.next() % n).expect("below the bound")
    }
}

/// Output of short lines, maybe with an unfinished last one, and its
/// lines with whether they ended.
fn random_output(rng: &mut Rng) -> (Vec<u8>, Vec<(Vec<u8>, bool)>) {
    let alphabet = ["a", "b", " ", "€", "ü", "\t"];
    let mut output = vec![];
    let mut lines = vec![];
    let count = rng.below(20);
    for i in 0..count {
        let mut line = vec![];
        for _ in 0..rng.below(40) {
            line.extend_from_slice(alphabet[rng.below(alphabet.len())].as_bytes());
        }
        output.extend_from_slice(&line);
        let ended = i + 1 < count || rng.below(2) == 0;
        if ended {
            output.push(b'\n');
        } else if line.is_empty() {
            break;
        }
        lines.push((line, ended));
    }
    (output, lines)
}

/// Split the output at random points, also inside of characters.
fn random_chunks<'a>(rng: &mut Rng, mut output: &'a [u8]) -> Vec<&'a [u8]> {
    let mut chunks = vec![];
    while !output.is_empty() {
        let (chunk, rest) = output.split_at(1 + rng.below(output.len().min(16)));
        chunks.push(chunk);
        output = rest;
    }
    chunks
}

#[tokio::test]
async fn any_split_gives_the_same_lines() -> Result<()> {
    for seed in 1..=500 {
        let mut rng = Rng(seed);
        let (output, expected) = random_output(&mut rng);
        let chunks = random_chunks(&mut rng, &output);

        // The drivers which take whole lines.
        let mut buffer = LineBuffer::default();
        let mut lines = vec![];
        for chunk in &chunks {
            lines.extend(buffer.push(Pipe::StdOut, chunk));
        }
        lines.extend(buffer.finish(Pipe::StdOut));
        let lines: Vec<_> = lines
            .into_iter()
            .map(|line| (line.text.to_vec(), !line.partial))
            .collect();
        assert_eq!(lines, expected, "seed {seed}");

        // The CRI log writes the chunks as they come, a reader joins the
        // partial entries with the ones after them.
        let (file, mut cri) = test_helpers::cri_logger(None).await?;
        for chunk in &chunks {
            cri.write_chunk(Pipe::StdOut, chunk).await?;
        }
        let log = fs::read(file.path())?;
        let mut joined = vec![];
        let mut line = vec![];
        for entry in log.split_inclusive(|b| *b == b'\n') {
            let entry = entry
                .strip_prefix(format!("{} stdout ", test_helpers::TIMESTAMP).as_bytes())
                .context("entry prefix")?;
            let (tag, text) = entry.split_at(2);
            line.extend_from_slice(text.strip_suffix(b"\n").context("entry end")?);
            if tag == b"F " {
                joined.push((std::mem::take(&mut line), true));
            }
        }
        if !line.is_empty() {
            joined.push((line, false));
        }
        assert_eq!(joined, expected, "seed {seed}");
    }
    Ok(())
}

#[test]
fn lines_split_a_chunk() {
    let got: Vec<_> = lines(b"a\nb").collect();
    assert_eq!(got, vec![(&b"a\n"[..], false), (&b"b"[..], true)]);
    let none: [(&[u8], bool); 0] = [];
    for (chunk, expected) in [
        (&b""[..], &none[..]),
        (b"a", &[(&b"a"[..], true)]),
        (b"a\n", &[(&b"a\n"[..], false)]),
        (b"\n", &[(&b"\n"[..], false)]),
        (b"\n\n", &[(&b"\n"[..], false), (&b"\n"[..], false)]),
        (b"a\r\nb\n", &[(&b"a\r\n"[..], false), (&b"b\n"[..], false)]),
    ] {
        let got: Vec<_> = lines(chunk).collect();
        assert_eq!(got, expected, "{chunk:?}");
        // Like splitting after every newline.
        let split: Vec<_> = chunk
            .split_inclusive(|&b| b == b'\n')
            .map(|line| (line, !line.ends_with(b"\n")))
            .collect();
        assert_eq!(got, split, "{chunk:?}");
    }
    assert_eq!(strip_line_ending(b"x\r\n"), b"x");
    assert_eq!(strip_line_ending(b"x\n"), b"x");
    assert_eq!(strip_line_ending(b"x"), b"x");
}

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
    assert!(sut.commit_entry(6).await.is_err());

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

#[test]
fn log_paths_have_to_be_absolute() -> Result<()> {
    use crate::conmon_capnp::conmon::create_container_request;

    let mut message = capnp::message::Builder::new_default();
    let mut drivers = message
        .init_root::<create_container_request::Builder<'_>>()
        .init_log_drivers(2);
    for (i, path) in ["/log", "log"].into_iter().enumerate() {
        let mut driver = drivers.reborrow().get(u32::try_from(i)?);
        driver.set_type(Type::Json);
        driver.set_path(path);
    }
    let drivers = drivers.into_reader();
    assert!(ContainerLog::path(&drivers.get(0)).is_ok());
    let err = ContainerLog::path(&drivers.get(1)).unwrap_err();
    assert!(err.to_string().contains("is not absolute"), "{err}");
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
    assert!(sut.commit_entry(0).await.is_err());
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
    assert!(sut.flush().await.is_err());
    assert!(sut.buf.is_empty());
    assert_eq!(sut.bytes_written, 0);

    sut.open().await?;
    sut.buf.extend_from_slice(b"kept\n");
    sut.flush().await?;
    assert_eq!(fs::read_to_string(file.path())?, "kept\n");
    Ok(())
}

#[tokio::test]
async fn drivers_share_the_output() -> Result<()> {
    let (cri, json, log) = test_helpers::container_log(true).await?;
    let mut log = log.lock().await;

    log.write(Pipe::StdOut, b"a\nparti").await?;
    log.write(Pipe::StdOut, b"al\n").await?;
    log.finish(Pipe::StdOut).await?;

    // The CRI log writes partial lines as they come, the JSON one joins
    // them.
    let ts = test_helpers::TIMESTAMP;
    assert_eq!(
        test_helpers::read(&cri)?,
        format!("{ts} stdout F a\n{ts} stdout P parti\n{ts} stdout F al\n")
    );
    let json = test_helpers::read(&json)?;
    let messages: Vec<_> = json
        .lines()
        .map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line)?;
            Ok(entry["message"].as_str().unwrap_or_default().to_owned())
        })
        .collect::<Result<_>>()?;
    assert_eq!(messages, ["a", "partial"]);
    Ok(())
}

#[tokio::test]
async fn failing_driver_does_not_stop_the_others() -> Result<()> {
    let (cri, json, log) = test_helpers::container_log(false).await?;
    let mut log = log.lock().await;
    // Only the JSON log gets opened, the CRI one fails every write.
    if let Driver::Json(json) = &mut log.drivers[1] {
        json.init().await?;
    }

    assert!(log.write(Pipe::StdOut, b"line\n").await.is_err());
    assert!(test_helpers::read(&cri)?.is_empty());
    assert!(test_helpers::read(&json)?.contains(r#""message":"line""#));
    Ok(())
}

#[tokio::test]
async fn reopen_reopens_all_drivers() -> Result<()> {
    let (cri, json, log) = test_helpers::container_log(true).await?;
    let mut log = log.lock().await;
    log.write(Pipe::StdOut, b"before\n").await?;

    // The CRI log cannot be opened again.
    fs::remove_file(cri.path())?;
    fs::create_dir(cri.path())?;

    assert!(log.reopen().await.is_err());
    assert!(test_helpers::read(&json)?.is_empty());
    fs::remove_dir(cri.path())?;
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
    assert!(sut.open().await.is_err());
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
async fn buffer_is_allocated_on_first_use() -> Result<()> {
    let file = NamedTempFile::new()?;
    let mut sut = LogFile::new(file.path(), None, None);
    sut.open().await?;
    assert_eq!(sut.buf.capacity(), 0);

    sut.buf().extend_from_slice(b"line\n");
    sut.flush().await?;
    assert_eq!(fs::read_to_string(file.path())?, "line\n");
    assert!(sut.buf.capacity() >= LogFile::BUF_CAPACITY);
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
    sut.flush().await?;
    assert!(sut.buf.capacity() <= LogFile::BUF_RETAIN_LIMIT);
    assert_eq!(fs::read_to_string(file.path())?, expected);
    Ok(())
}
