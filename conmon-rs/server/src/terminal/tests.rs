use super::*;
use crate::{
    attach::SharedContainerAttach,
    test_util::{open_pty, run_in_own_process, send_fd},
};
use nix::{
    fcntl::FdFlag,
    sys::{
        resource::{Resource, getrlimit, setrlimit},
        socket::{ControlMessage, MsgFlags, sendmsg},
    },
    unistd::pipe2,
};
use std::{
    fs::File,
    io::IoSlice,
    os::unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    time::Duration,
};

/// The width and height of a terminal.
#[expect(unsafe_code, reason = "the TIOCGWINSZ ioctl")]
fn size(fd: BorrowedFd<'_>) -> Result<(u16, u16)> {
    let mut size = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a `winsize` to the pointer, which points
    // to a live local of exactly that type.
    match unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } {
        0 => Ok((size.ws_col, size.ws_row)),
        _ => Err(io::Error::last_os_error().into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_success() -> Result<()> {
    let mut sut = Terminal::new()?;
    assert!(sut.path().exists());
    // Only the owner may connect.
    assert_eq!(
        std::fs::metadata(sut.path())?.permissions().mode() & 0o077,
        0
    );

    let res = open_pty()?;

    let mut stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"test", res.master.as_fd()).await?;

    let _io = sut.connect().await?;
    // The listener removes the socket after handing over the terminal,
    // before it shuts down the stream, and resets it once dropped, as it
    // did not read the data.
    match tokio::io::AsyncReadExt::read(&mut stream, &mut [0; 1]).await {
        Ok(0) => {}
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        res => anyhow::bail!("stream did not end: {res:?}"),
    }
    assert!(!sut.path().exists());

    // The received terminal does not leak into children.
    let tty = sut.tty().context("no tty")?;
    let flags = FdFlag::from_bits_truncate(fcntl::fcntl(&*tty, FcntlArg::F_GETFD)?);
    assert!(flags.contains(FdFlag::FD_CLOEXEC));

    sut.resize(120, 40)?;
    assert_eq!(size(res.slave.as_fd())?, (120, 40));

    // Write to the slave
    let mut file: std::fs::File = res.slave.into();
    file.write_all(b"test")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn size_before_the_connection_gets_applied() -> Result<()> {
    let mut sut = Terminal::new()?;
    // Like a client sending its size while the runtime starts the process,
    // through a handle taken before, the latest size wins.
    let resizer = sut.resizer();
    resizer.resize(80, 24)?;
    resizer.resize(132, 50)?;
    assert_eq!(sut.resizer.pending(), Some((132, 50)));

    let res = open_pty()?;
    assert_eq!(size(res.slave.as_fd())?, (0, 0));
    let stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    let io = sut.connect().await?;
    assert_eq!(size(res.slave.as_fd())?, (132, 50));

    // Later resizes go to the terminal right away, until it is closed.
    resizer.resize(100, 30)?;
    assert_eq!(size(res.slave.as_fd())?, (100, 30));
    drop(io);
    assert!(resizer.resize(100, 30).is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_char_ends_the_input() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let mut sut = Terminal::new()?;
    let res = open_pty()?;
    let stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    let io = sut.connect().await?;
    task::spawn(crate::container_io::ContainerIO::read_loop_stdin(
        io,
        attach.clone(),
        tokio_util::sync::CancellationToken::new(),
    ));

    let eof = sut.eof_char();
    assert_eq!(eof, 0x04);
    attach.send_stdin(vec![eof].into()).await;

    // A process reading the terminal sees the end of its input.
    let mut slave: std::fs::File = res.slave.into();
    let read = task::spawn_blocking(move || io::Read::read(&mut slave, &mut [0; 16]));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), read).await???,
        0
    );
    Ok(())
}

/// The end of file character is the one of the terminal, which a process
/// may change.
#[tokio::test(flavor = "multi_thread")]
async fn eof_char_follows_the_terminal_settings() -> Result<()> {
    let mut sut = Terminal::new()?;
    // The default before the runtime connected the terminal.
    assert_eq!(sut.eof_char(), 0x04);
    let res = open_pty()?;
    let stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    let _io = sut.connect().await?;
    assert_eq!(sut.eof_char(), 0x04);

    let mut term = termios::tcgetattr(&res.slave)?;
    term.control_chars[SpecialCharacterIndices::VEOF as usize] = 0x1a;
    termios::tcsetattr(&res.slave, SetArg::TCSANOW, &term)?;
    assert_eq!(sut.eof_char(), 0x1a);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_socket_does_not_drop_the_terminal() -> Result<()> {
    let mut sut = Terminal::new()?;
    // Short enough for runc, which dials it by its path.
    assert!(
        sut.path().as_os_str().len() < 108,
        "{}",
        sut.path().display()
    );
    let res = open_pty()?;
    let stream = UnixStream::connect(sut.path()).await?;
    // Like a cleaner of the directory, the connection is up already.
    std::fs::remove_file(sut.path())?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    tokio::time::timeout(Duration::from_secs(5), sut.connect()).await??;
    Ok(())
}

#[tokio::test]
async fn listen_stops_without_terminal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("term.sock");
    let listener = UnixListener::bind(&path)?;
    let (connected_tx, connected_rx) = mpsc::channel(1);
    let config = Config { path, connected_tx };

    // A runtime which connects but never sends the descriptor must not
    // keep the listener around either once the terminal side is gone.
    let listen = task::spawn(Terminal::listen(listener, config));
    let _stream = UnixStream::connect(dir.path().join("term.sock")).await?;
    drop(connected_rx);
    tokio::time::timeout(Duration::from_secs(5), listen).await???;
    Ok(())
}

const RIGHTS_CLEANUP_TEST_ENV: &str = "CONMONRS_RIGHTS_CLEANUP_TEST";

#[test]
fn receiving_rights_closes_extras_and_sets_cloexec() -> Result<()> {
    run_in_own_process(
        "terminal::tests::receiving_rights_in_own_process",
        RIGHTS_CLEANUP_TEST_ENV,
        false,
    )
}

#[test]
#[ignore = "run by receiving_rights_closes_extras_and_sets_cloexec in a process of its own"]
fn receiving_rights_in_own_process() -> Result<()> {
    if std::env::var_os(RIGHTS_CLEANUP_TEST_ENV).is_none() {
        return Ok(());
    }
    // EOF counts every writer, including copies inherited by a concurrent
    // fork before exec closes CLOEXEC descriptors. Keep this assertion in
    // a process which runs no other tests and does not spawn children.
    let (sender, receiver) = StdUnixStream::pair()?;
    let (read, write) = pipe2(OFlag::O_CLOEXEC)?;
    let rights = [write.as_raw_fd(); 3];
    sendmsg::<()>(
        sender.as_raw_fd(),
        &[IoSlice::new(b"x")],
        &[ControlMessage::ScmRights(&rights)],
        MsgFlags::empty(),
        None,
    )?;
    drop(write);
    let received = Terminal::recv_fd(receiver.as_fd())?.context("received rights")?;
    let flags = FdFlag::from_bits_truncate(fcntl::fcntl(&received, FcntlArg::F_GETFD)?);
    assert!(flags.contains(FdFlag::FD_CLOEXEC));
    drop(received);
    // EOF proves that the other received copies were closed as well.
    fcntl::fcntl(&read, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    let mut read = File::from(read);
    assert_eq!(read.read(&mut [0; 1])?, 0);

    let (mut sender, receiver) = StdUnixStream::pair()?;
    sender.write_all(b"no rights")?;
    assert!(Terminal::recv_fd(receiver.as_fd())?.is_none());
    Ok(())
}

const RIGHTS_LIMIT_TEST_ENV: &str = "CONMONRS_RIGHTS_LIMIT_TEST";

#[test]
fn truncated_rights_do_not_leak_descriptors() -> Result<()> {
    run_in_own_process(
        "terminal::tests::truncated_rights_in_own_process",
        RIGHTS_LIMIT_TEST_ENV,
        false,
    )
}

#[test]
#[ignore = "run by truncated_rights_do_not_leak_descriptors in a process of its own"]
fn truncated_rights_in_own_process() -> Result<()> {
    if std::env::var_os(RIGHTS_LIMIT_TEST_ENV).is_none() {
        return Ok(());
    }
    let (sender, receiver) = StdUnixStream::pair()?;
    let file = File::open("/dev/null")?;
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
    setrlimit(Resource::RLIMIT_NOFILE, soft.min(64), hard)?;
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(e) if e.raw_os_error() == Some(libc::EMFILE) => break,
            Err(e) => return Err(e.into()),
        }
    }
    // Leave only two slots for three rights. The kernel installs two,
    // discards the third and sets MSG_CTRUNC, regardless of buffer space.
    assert!(fillers.len() >= 2);
    drop(fillers.pop());
    drop(fillers.pop());
    sendmsg::<()>(
        sender.as_raw_fd(),
        &[IoSlice::new(b"x")],
        &[ControlMessage::ScmRights(&[file.as_raw_fd(); 3])],
        MsgFlags::empty(),
        None,
    )?;
    let received = Terminal::recv_fd(receiver.as_fd());
    let first = File::open("/dev/null");
    let second = File::open("/dev/null");
    let third = File::open("/dev/null");
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard)?;
    assert_eq!(received.unwrap_err().raw_os_error(), Some(libc::ENOBUFS));
    assert!(first.is_ok() && second.is_ok(), "partial rights leaked");
    assert_eq!(third.unwrap_err().raw_os_error(), Some(libc::EMFILE));
    Ok(())
}
