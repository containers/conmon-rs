use super::{testing::decode, *};
use nix::sys::stat::{self, Mode, SFlag};

#[test]
fn priorities_follow_the_level() {
    assert_eq!(Journal::priority_for(Level::ERROR), Priority::Error);
    assert_eq!(Journal::priority_for(Level::WARN), Priority::Warning);
    assert_eq!(Journal::priority_for(Level::INFO), Priority::Info);
    assert_eq!(Journal::priority_for(Level::DEBUG), Priority::Debug);
    assert_eq!(Journal::priority_for(Level::TRACE), Priority::Debug);
}

#[test]
fn writer_drops_the_line_ending() {
    let journal = Journal {
        priority: Some(Priority::Warning),
    };
    assert_eq!(
        journal.entry(b"event\n").as_bytes(),
        b"PRIORITY=4\nMESSAGE=event\nSYSLOG_IDENTIFIER=conmonrs\n"
    );
    assert_eq!(
        decode(journal.entry(b"two\nlines\n").as_bytes())[1],
        ("MESSAGE".into(), b"two\nlines".to_vec())
    );
    assert_eq!(
        Journal::default().entry(b"plain").as_bytes(),
        b"PRIORITY=5\nMESSAGE=plain\nSYSLOG_IDENTIFIER=conmonrs\n"
    );
}

#[test]
fn early_errors_of_the_server_have_their_priority() {
    assert_eq!(
        Journal::error().entry(b"bind socket").as_bytes(),
        b"PRIORITY=3\nMESSAGE=bind socket\nSYSLOG_IDENTIFIER=conmonrs\n"
    );
}

#[test]
fn extend_appends_encoded_fields() {
    let mut fields = Entry::default();
    fields.field("A", "1").field("B", "x\ny");
    let mut entry = Entry::new(Priority::Info, "m");
    entry.extend(&fields).field("C", "3");
    assert_eq!(
        decode(entry.as_bytes()),
        [
            ("PRIORITY".into(), b"6".to_vec()),
            ("MESSAGE".into(), b"m".to_vec()),
            ("A".into(), b"1".to_vec()),
            ("B".into(), b"x\ny".to_vec()),
            ("C".into(), b"3".to_vec())
        ]
    );
}

#[test]
fn simple_fields() {
    let mut entry = Entry::new(Priority::Info, "hello");
    entry.field("CONTAINER_ID", "abc").field("EMPTY", "");
    assert_eq!(
        entry.as_bytes(),
        b"PRIORITY=6\nMESSAGE=hello\nCONTAINER_ID=abc\nEMPTY=\n"
    );
}

#[test]
fn values_with_newlines_carry_their_length() {
    let entry = Entry::new(Priority::Error, "a\nb=c\n");
    let mut expected = b"PRIORITY=3\nMESSAGE\n".to_vec();
    expected.extend_from_slice(&6u64.to_le_bytes());
    expected.extend_from_slice(b"a\nb=c\n\n");
    assert_eq!(entry.as_bytes(), expected);
    assert_eq!(
        decode(entry.as_bytes()),
        [
            ("PRIORITY".into(), b"3".to_vec()),
            ("MESSAGE".into(), b"a\nb=c\n".to_vec())
        ]
    );
}

#[test]
fn binary_values_round_trip() {
    let value = [0u8, b'\n', 0xff, b'='];
    let mut entry = Entry::new(Priority::Debug, [0xffu8, 0xfe]);
    entry.field("BLOB", value);
    assert_eq!(
        decode(entry.as_bytes()),
        [
            ("PRIORITY".into(), b"7".to_vec()),
            ("MESSAGE".into(), vec![0xff, 0xfe]),
            ("BLOB".into(), value.to_vec())
        ]
    );
}

#[test]
fn valid_keys() {
    for key in ["MESSAGE", "CONTAINER_ID_FULL", "A1", "X_"] {
        assert!(Entry::valid_key(key), "{key}");
    }
    let long = "A".repeat(65);
    for key in ["", "_PID", "1A", "lower", "WITH-DASH", "WITH=EQ", &long] {
        assert!(!Entry::valid_key(key), "{key}");
    }
}

/// Receive one datagram, with its content or the content of the memfd it
/// carries.
fn receive(receiver: &UnixDatagram) -> anyhow::Result<(Vec<u8>, bool)> {
    use nix::sys::socket::{ControlMessageOwned, recvmsg};
    use std::{
        io::{IoSliceMut, Read, Seek},
        os::fd::{FromRawFd, RawFd},
    };

    let mut buf = vec![0; 4096];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg = nix::cmsg_space!([RawFd; 1]);
    let msg = recvmsg::<()>(
        receiver.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )?;
    let len = msg.bytes;
    let Some(ControlMessageOwned::ScmRights(fds)) = msg.cmsgs()?.next() else {
        buf.truncate(len);
        return Ok((buf, false));
    };
    #[expect(unsafe_code, reason = "taking a received descriptor")]
    // SAFETY: SCM_RIGHTS installed a new descriptor which nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fds[0]) };
    let seals = fcntl(&file, FcntlArg::F_GET_SEALS)?;
    assert!(
        SealFlag::from_bits_truncate(seals)
            .contains(SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_WRITE)
    );
    // The descriptor shares the offset with the one of the sender, which
    // is at the end. journald maps the memfd instead.
    file.rewind()?;
    let mut data = vec![];
    file.read_to_end(&mut data)?;
    Ok((data, true))
}

/// Entries go inline if they fit into a datagram, otherwise as memfd.
#[test]
fn send_inline_and_as_memfd() -> anyhow::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let receiver = UnixDatagram::bind(&path)?;
    let sender = UnixDatagram::unbound()?;
    // A small send buffer makes the large entry fail inline, whatever
    // the default of the host is.
    setsockopt(&sender, sockopt::SndBuf, &4096)?;

    let small = Entry::new(Priority::Info, "small");
    send_to(&sender, &path, small.as_bytes(), None)?;
    assert_eq!(receive(&receiver)?, (small.as_bytes().to_vec(), false));

    let large = Entry::new(Priority::Info, "x".repeat(1 << 20));
    send_to(&sender, &path, large.as_bytes(), None)?;
    let (data, memfd) = receive(&receiver)?;
    assert!(memfd);
    assert!(data == large.as_bytes(), "memfd holds the entry");
    Ok(())
}

/// Send small entries to a socket which is never read, until the queue
/// of the receiver is full and sending fails.
///
/// The entries take up the send buffer of the socket they get sent on
/// until they are received, which may fill up before the queue of the
/// receiver does. So they are sent on a socket of their own, the shared
/// ones of the other tests would fail every send meanwhile otherwise.
pub fn fill(wait: Wait, path: &Path) -> io::Error {
    let socket = match new_socket(wait) {
        Ok(socket) => socket,
        Err(e) => return e,
    };
    let entry = Entry::new(Priority::Info, "fill");
    for _ in 0..100_000 {
        if let Err(e) = send_on(&socket, wait, path, &entry) {
            return e;
        }
    }
    panic!("the receiver queue never filled up");
}

/// The tracing writer drops lines instead of blocking the runtime thread
/// when the journal does not keep up.
#[test]
fn writer_never_blocks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let _receiver = UnixDatagram::bind(&path)?;

    let error = fill(Wait::Never, &path);
    assert_eq!(error.kind(), ErrorKind::WouldBlock);

    let journal = Journal::default();
    let start = std::time::Instant::now();
    for _ in 0..100 {
        journal.send_at(&path, b"dropped\n")?;
    }
    // Blocking sends would take a `SEND_TIMEOUT` each.
    assert!(start.elapsed() < 10 * SEND_TIMEOUT);
    Ok(())
}

/// The server logs do not share a send buffer with the container
/// output.
#[test]
fn server_logs_have_a_socket_of_their_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let _receiver = UnixDatagram::bind(&path)?;

    Journal::default().send_at(&path, b"server\n")?;
    send_at(Wait::Never, &path, &Entry::new(Priority::Info, "container"))?;
    let (Some(server), Some(container)) = (SERVER_SOCKET.get(), NEVER_SOCKET.get()) else {
        panic!("sockets not created");
    };
    assert!(!std::ptr::eq(server, container));
    Ok(())
}

/// The container logs wait for a stalled journal, but not forever.
#[test]
fn bounded_send_times_out() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let _receiver = UnixDatagram::bind(&path)?;

    let start = std::time::Instant::now();
    let error = fill(Wait::Bounded(Instant::now() + 60 * SEND_TIMEOUT), &path);
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert!(start.elapsed() >= SEND_TIMEOUT);
    Ok(())
}

/// A missing socket and a socket nobody listens on mean that there is
/// no journal, which the tracing writer ignores.
#[test]
fn writer_ignores_a_missing_journal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing");
    let stale = dir.path().join("stale");
    // A socket inode which was never bound, like the one of a journald
    // which exited. No descriptor of it exists, which a child forked by a
    // parallel test could hold.
    stat::mknod(&stale, SFlag::S_IFSOCK, Mode::S_IRUSR | Mode::S_IWUSR, 0)?;

    let entry = Entry::new(Priority::Info, "lost");
    for path in [&missing, &stale] {
        let error = send_at(Wait::Never, path, &entry).expect_err("no journal");
        assert!(unavailable(&error), "{error}");
        Journal::default().send_at(path, b"lost\n")?;
    }
    Ok(())
}

/// A panic reaches the journal as an error with its thread and location,
/// also with a message which spans several lines.
#[test]
fn panics_get_logged() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let receiver = UnixDatagram::bind(&path)?;
    let id = Pid::from_raw(42);

    log_panic_at(&path, Some("main"), id, &"panicked at src/a.rs:1:2:\nboom")?;
    let (data, _) = receive(&receiver)?;
    assert_eq!(
        decode(&data),
        [
            ("PRIORITY".into(), b"3".to_vec()),
            (
                "MESSAGE".into(),
                b"thread 'main' (42) panicked at src/a.rs:1:2:\nboom".to_vec()
            ),
            ("SYSLOG_IDENTIFIER".into(), IDENTIFIER.into())
        ]
    );

    log_panic_at(&path, None, id, &"panicked at src/a.rs:1:2:")?;
    let (data, _) = receive(&receiver)?;
    assert_eq!(
        decode(&data)[1],
        (
            "MESSAGE".into(),
            b"thread '<unnamed>' (42) panicked at src/a.rs:1:2:".to_vec()
        )
    );
    Ok(())
}

/// The thread name comes from the kernel, which keeps the start of it.
#[test]
fn panics_name_their_thread() -> anyhow::Result<()> {
    let name = std::thread::Builder::new()
        .name("a-long-thread-name".into())
        .spawn(thread_name)?
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?;
    assert_eq!(name.as_deref(), Some("a-long-thread-n"));
    Ok(())
}

/// The message of a panic waits for a journal which does not take it
/// right away, the process aborts after it.
#[test]
fn panics_wait_for_a_busy_journal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("socket");
    let receiver = UnixDatagram::bind(&path)?;
    let error = fill(Wait::Never, &path);
    assert_eq!(error.kind(), ErrorKind::WouldBlock);

    // The journal catches up a little later.
    let reader = std::thread::spawn(move || -> anyhow::Result<bool> {
        std::thread::sleep(Duration::from_millis(100));
        receiver.set_read_timeout(Some(Duration::from_secs(5)))?;
        loop {
            let (data, _) = receive(&receiver)?;
            if decode(&data)[1].1.ends_with(b"late") {
                return Ok(true);
            }
        }
    });
    log_panic_at(&path, None, gettid(), &"late")?;
    assert!(
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("reader panicked"))??
    );
    Ok(())
}
