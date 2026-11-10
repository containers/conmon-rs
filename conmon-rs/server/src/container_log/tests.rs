use super::*;
use std::fs;

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
        let (file, mut cri) = testing::cri_logger(None).await?;
        for chunk in &chunks {
            cri.write_chunk(Pipe::StdOut, chunk).await?;
        }
        let log = fs::read(file.path())?;
        let mut joined = vec![];
        let mut line = vec![];
        for entry in log.split_inclusive(|b| *b == b'\n') {
            let entry = entry
                .strip_prefix(format!("{} stdout ", testing::TIMESTAMP).as_bytes())
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

#[test]
fn log_paths_have_to_be_absolute() -> Result<()> {
    use crate::conmon_capnp::conmon::create_container_request;

    let mut message = capnp::message::Builder::new_default();
    let mut drivers = message
        .init_root::<create_container_request::Builder<'_>>()
        .init_log_drivers(2);
    for (i, path) in ["/log", "log"].into_iter().enumerate() {
        let mut driver = drivers.reborrow().get(u32::try_from(i)?);
        driver.set_type(Type::ContainerRuntimeInterface);
        driver.set_path(path);
    }
    let drivers = drivers.into_reader();
    assert!(ContainerLog::path(&drivers.get(0)).is_ok());
    let err = ContainerLog::path(&drivers.get(1)).unwrap_err();
    assert!(err.to_string().contains("is not absolute"), "{err}");
    Ok(())
}

#[tokio::test]
async fn drivers_share_the_output() -> Result<()> {
    let (cri, lines, log) = testing::container_log(true).await?;
    let mut log = log.lock().await;

    log.write(Pipe::StdOut, b"a\nparti").await?;
    log.write(Pipe::StdOut, b"al\n").await?;
    log.finish(Pipe::StdOut).await?;

    // The CRI log writes partial lines as they come, the journald driver
    // joins them.
    let ts = testing::TIMESTAMP;
    assert_eq!(
        testing::read(&cri)?,
        format!("{ts} stdout F a\n{ts} stdout P parti\n{ts} stdout F al\n")
    );
    let lines = lines.read()?;
    let messages: Vec<_> = lines
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
    let (cri, lines, log) = testing::container_log(false).await?;
    let mut log = log.lock().await;
    // The CRI driver did not get opened and fails every write, the journald
    // one needs no opening.

    assert!(log.write(Pipe::StdOut, b"line\n").await.is_err());
    assert!(testing::read(&cri)?.is_empty());
    assert!(lines.read()?.contains(r#""message":"line""#));
    Ok(())
}

#[tokio::test]
async fn reopen_reopens_all_drivers() -> Result<()> {
    let (cri, _journal, log) = testing::container_log(false).await?;
    let (other, other_cri) = testing::cri_logger(None).await?;
    let mut log = log.lock().await;
    log.drivers[1] = Driver::Cri(other_cri);
    log.init().await?;
    log.write(Pipe::StdOut, b"before\n").await?;

    // The first log cannot be opened again, the second one still gets a new
    // file.
    fs::remove_file(cri.path())?;
    fs::create_dir(cri.path())?;

    let err = log
        .reopen()
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        err.starts_with(&format!("rename log file to '{}': ", cri.path().display())),
        "{err}"
    );
    assert!(testing::read(&other)?.is_empty());
    fs::remove_dir(cri.path())?;
    Ok(())
}

#[tokio::test]
async fn sync_skips_files_which_cannot_be_synced() -> Result<()> {
    // Nothing got opened, there is nothing to sync.
    let (_cri, _lines, log) = testing::container_log(false).await?;
    ContainerLog::sync(&log).await?;

    // A device takes no sync, the other files still get synced.
    let (_cri, lines, log) = testing::container_log(false).await?;
    {
        let mut log = log.lock().await;
        log.drivers[0] = Driver::Cri(cri::CriLogger::new("/dev/null", None));
        log.init().await?;
        log.write(Pipe::StdOut, b"line\n").await?;
    }
    ContainerLog::sync(&log).await?;
    assert!(lines.read()?.contains(r#""message":"line""#));
    Ok(())
}
