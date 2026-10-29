use super::*;
use crate::test_util::{retry_while_in_use, wait_for};
use std::{
    os::{
        linux::net::SocketAddrExt,
        unix::{fs::FileTypeExt, net::SocketAddr},
    },
    time::Duration,
};
use tempfile::tempdir;
use tokio::time;

fn socket_path(bundle: &Path) -> PathBuf {
    bundle.join(NotifySocket::DIR).join(NotifySocket::FILE)
}

async fn recv_string(host: &UnixDatagram) -> Result<String> {
    let mut buf = [0; NotifySocket::MAX_MESSAGE_SIZE + 1];
    let len = time::timeout(Duration::from_secs(5), host.recv(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
}

#[tokio::test]
async fn relays_messages_until_cancelled() -> Result<()> {
    let bundle = tempdir()?;
    let host_path = bundle.path().join("host.sock");
    let host = UnixDatagram::bind(&host_path)?;

    let sut = NotifySocket::bind(bundle.path(), host_path.to_str().context("path")?)?;
    let path = socket_path(bundle.path());
    assert_eq!(sut.path, path);
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o777);
    // It got bound next to the notify directory and moved into it.
    for entry in fs::read_dir(bundle.path())? {
        let name = entry?.file_name();
        assert!(
            !name.to_string_lossy().starts_with(NotifySocket::TMP_PREFIX),
            "{name:?}"
        );
    }

    let token = CancellationToken::new();
    sut.spawn(token.clone());

    let container = UnixDatagram::unbound()?;
    container.send_to(b"READY=1\nSTATUS=up", &path).await?;
    assert_eq!(recv_string(&host).await?, "READY=1\nSTATUS=up");

    // Only the allowed assignments get relayed, and nothing if none of
    // them is left.
    container
        .send_to(b"MAINPID=1\nREADY=1\nFDSTORE=1", &path)
        .await?;
    assert_eq!(recv_string(&host).await?, "READY=1");
    container.send_to(b"MAINPID=1\nBARRIER=1", &path).await?;
    container.send_to(b"STATUS=filtered", &path).await?;
    assert_eq!(recv_string(&host).await?, "STATUS=filtered");

    // Too long messages are dropped, the rest still arrives.
    let long = vec![b'x'; NotifySocket::MAX_MESSAGE_SIZE + 1];
    container.send_to(&long, &path).await?;
    container.send_to(b"STATUS=after", &path).await?;
    assert_eq!(recv_string(&host).await?, "STATUS=after");

    // A recreated host socket still receives.
    drop(host);
    fs::remove_file(&host_path)?;
    let host = UnixDatagram::bind(&host_path)?;
    container.send_to(b"STATUS=recreated", &path).await?;
    assert_eq!(recv_string(&host).await?, "STATUS=recreated");

    // Messages sent right before the exit are not lost.
    container.send_to(b"STOPPING=1", &path).await?;
    token.cancel();
    assert_eq!(recv_string(&host).await?, "STOPPING=1");

    wait_for(|| !path.exists()).await?;
    Ok(())
}

#[test]
fn filter_message() {
    for (input, expected) in [
        (&b"READY=1"[..], &b"READY=1"[..]),
        (b"READY=1\nSTATUS=up\n", b"READY=1\nSTATUS=up"),
        (
            b"RELOADING=1\r\nMONOTONIC_USEC=42",
            b"RELOADING=1\nMONOTONIC_USEC=42",
        ),
        (
            b"STOPPING=1\nERRNO=2\nBUSERROR=x.y",
            b"STOPPING=1\nERRNO=2\nBUSERROR=x.y",
        ),
        (
            b"WATCHDOG=1\nWATCHDOG=trigger",
            b"WATCHDOG=1\nWATCHDOG=trigger",
        ),
        (b"STATUS=\xff\xfe", b"STATUS=\xff\xfe"),
        (b"MAINPID=1\nREADY=1", b"READY=1"),
        (b"READY=10\nREADY=1 \nWATCHDOG=0\nWATCHDOG_USEC=1", b""),
        (b"NOTIFYACCESS=all\nFDSTORE=1\nBARRIER=1", b""),
        (b"EXTEND_TIMEOUT_USEC=1\nWATCHDOG_USEC=1\nFDNAME=x", b""),
        (b"status=up\n\n", b""),
        (b"", b""),
    ] {
        assert_eq!(
            NotifySocket::filter_message(input),
            expected,
            "{:?}",
            String::from_utf8_lossy(input)
        );
    }
}

#[tokio::test]
async fn relays_to_an_abstract_host_socket() -> Result<()> {
    let bundle = tempdir()?;
    let name = format!("conmonrs-test-{}", std::process::id());
    let host = StdUnixDatagram::bind_addr(&SocketAddr::from_abstract_name(&name)?)?;
    host.set_nonblocking(true)?;
    let host = UnixDatagram::from_std(host)?;

    let sut = NotifySocket::bind(bundle.path(), &format!("@{name}"))?;
    let path = socket_path(bundle.path());
    let token = CancellationToken::new();
    sut.spawn(token.clone());

    let container = UnixDatagram::unbound()?;
    container.send_to(b"READY=1", &path).await?;
    assert_eq!(recv_string(&host).await?, "READY=1");

    token.cancel();
    Ok(())
}

#[tokio::test]
async fn bind_fails_without_host_socket() -> Result<()> {
    let bundle = tempdir()?;
    let host_path = bundle.path().join("missing.sock");
    let host = host_path.to_str().context("path")?;
    assert!(NotifySocket::bind(bundle.path(), host).is_err());
    assert!(!socket_path(bundle.path()).exists());
    Ok(())
}

#[tokio::test]
async fn bind_replaces_a_stale_socket_but_not_a_live_one() -> Result<()> {
    let bundle = tempdir()?;
    let host_path = bundle.path().join("host.sock");
    let _host = UnixDatagram::bind(&host_path)?;
    let host = host_path.to_str().context("path")?;
    let path = socket_path(bundle.path());
    fs::create_dir(bundle.path().join(NotifySocket::DIR))?;
    fs::write(&path, b"stale")?;

    let first = NotifySocket::bind(bundle.path(), host)?;
    assert!(fs::metadata(&path)?.file_type().is_socket());

    // Another container of the bundle still uses the socket.
    assert!(NotifySocket::bind(bundle.path(), host).is_err());
    assert!(path.exists());

    drop(first);
    assert!(!path.exists());

    // The socket of a server which exited without cleaning up. A process
    // which a parallel test forks may hold it until it execs.
    let stale = StdUnixDatagram::bind(&path)?;
    drop(stale);
    let again = retry_while_in_use(|| NotifySocket::bind(bundle.path(), host)).await?;
    drop(again);
    assert!(!path.exists());
    Ok(())
}
