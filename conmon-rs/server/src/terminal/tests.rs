use super::*;
use crate::container_log::ContainerLog;
use nix::{fcntl::FdFlag, pty, sys::socket::ControlMessage};
use std::{io::IoSlice, os::unix::fs::PermissionsExt, time::Duration};

/// Send the terminal descriptor with some data, like a runtime does.
async fn send_fd(stream: &UnixStream, data: &[u8], fd: BorrowedFd<'_>) -> Result<()> {
    let fds = [fd.as_raw_fd()];
    stream
        .async_io(Interest::WRITABLE, || {
            socket::sendmsg::<()>(
                stream.as_raw_fd(),
                &[IoSlice::new(data)],
                &[ControlMessage::ScmRights(&fds)],
                MsgFlags::empty(),
                None,
            )
            .map_err(io::Error::from)
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[expect(unsafe_code, reason = "the TIOCGWINSZ ioctl")]
async fn new_success() -> Result<()> {
    let logger = ContainerLog::new();
    let attach = SharedContainerAttach::default();
    let token = CancellationToken::new();

    let mut sut = Terminal::new(logger, attach)?;
    assert!(sut.path().exists());
    // Only the owner may connect.
    assert_eq!(
        std::fs::metadata(sut.path())?.permissions().mode() & 0o077,
        0
    );

    let res = pty::openpty(None, None)?;

    let stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"test", res.master.as_fd()).await?;

    sut.wait_connected(true, token).await?;
    assert!(!sut.path().exists());

    // The received terminal does not leak into children.
    let tty = sut
        .tty()
        .as_ref()
        .and_then(Weak::upgrade)
        .context("no tty")?;
    let flags = FdFlag::from_bits_truncate(fcntl::fcntl(&*tty, FcntlArg::F_GETFD)?);
    assert!(flags.contains(FdFlag::FD_CLOEXEC));

    sut.resize(120, 40)?;
    let mut size = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a `winsize` to the pointer, which points
    // to a live local of exactly that type.
    let ret = unsafe { libc::ioctl(res.slave.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    assert_eq!(ret, 0);
    assert_eq!((size.ws_col, size.ws_row), (120, 40));

    // Write to the slave
    let mut file: std::fs::File = res.slave.into();
    file.write_all(b"test")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_char_ends_the_input() -> Result<()> {
    let attach = SharedContainerAttach::default();
    let mut sut = Terminal::new(ContainerLog::new(), attach.clone())?;
    let res = pty::openpty(None, None)?;
    let stream = UnixStream::connect(sut.path()).await?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    sut.wait_connected(true, CancellationToken::new()).await?;

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

#[tokio::test(flavor = "multi_thread")]
async fn removed_socket_does_not_drop_the_terminal() -> Result<()> {
    let mut sut = Terminal::new(ContainerLog::new(), SharedContainerAttach::default())?;
    // Short enough for runc, which dials it by its path.
    assert!(
        sut.path().as_os_str().len() < 108,
        "{}",
        sut.path().display()
    );
    let res = pty::openpty(None, None)?;
    let stream = UnixStream::connect(sut.path()).await?;
    // Like a cleaner of the directory, the connection is up already.
    std::fs::remove_file(sut.path())?;
    send_fd(&stream, b"x", res.master.as_fd()).await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        sut.wait_connected(false, CancellationToken::new()),
    )
    .await??;
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
    let listen = tokio::spawn(Terminal::listen(listener, config));
    let _stream = UnixStream::connect(dir.path().join("term.sock")).await?;
    drop(connected_rx);
    tokio::time::timeout(Duration::from_secs(5), listen).await???;
    Ok(())
}
