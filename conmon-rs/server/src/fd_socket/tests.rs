use super::*;
use crate::test_util::wait_for;
use std::{fs::File, io::IoSlice, os::fd::AsFd};
use tokio_seqpacket::ancillary::AncillaryMessageWriter;

/// Send a request for `num_fds` with the given descriptors attached and
/// return the response.
async fn request(conn: &UnixSeqpacket, id: u64, num_fds: u64, fds: &[OwnedFd]) -> Result<Vec<u8>> {
    send(conn, &((id << 32) | num_fds).to_le_bytes(), fds).await?;
    let mut buf = [0; 8 + MAX_FDS * 8 + 1];
    let n = conn.recv(&mut buf).await?.bytes_read();
    Ok(buf[..n].to_vec())
}

/// Send a message with the given descriptors attached.
async fn send(conn: &UnixSeqpacket, message: &[u8], fds: &[OwnedFd]) -> Result<()> {
    let mut ancillary_buf = [0; ANCILLARY_BUF_SIZE];
    let mut ancillary = AncillaryMessageWriter::new(&mut ancillary_buf);
    ancillary.add_fds(fds.iter().map(AsFd::as_fd))?;
    conn.send_vectored_with_ancillary(&[IoSlice::new(message)], &mut ancillary)
        .await?;
    Ok(())
}

/// Start an fd socket and connect to it.
async fn connect() -> Result<(tempfile::TempDir, Arc<FdSocket>, UnixSeqpacket)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fd.sock");
    let fd_socket = Arc::new(FdSocket::default());
    fd_socket.clone().start(path.clone()).await?;
    let conn = UnixSeqpacket::connect(&path).await?;
    Ok((dir, fd_socket, conn))
}

fn null_fds(n: usize) -> Result<Vec<OwnedFd>> {
    (0..n)
        .map(|_| Ok(File::open("/dev/null")?.into()))
        .collect()
}

/// Whether the slot still holds a descriptor, without taking it.
fn has_slot(fd_socket: &FdSocket, slot: u64) -> bool {
    lock(&fd_socket.state).fds.contains_key(&slot)
}

#[tokio::test]
async fn empty_request_releases_the_slots() -> Result<()> {
    let (_dir, fd_socket, conn) = connect().await?;
    let resp = request(&conn, 1, 2, &null_fds(2)?).await?;
    let slots = [word(&resp, 1), word(&resp, 2)];

    // An empty request gets no response, the next one tells that it got
    // handled.
    send(&conn, &0u64.to_le_bytes(), &[]).await?;
    let resp = request(&conn, 2, 1, &null_fds(1)?).await?;
    assert_eq!(word(&resp, 0), (2 << 32) | 1);
    assert!(slots.iter().all(|slot| !has_slot(&fd_socket, *slot)));
    assert!(has_slot(&fd_socket, word(&resp, 1)));
    Ok(())
}

#[tokio::test]
async fn disconnect_releases_the_slots() -> Result<()> {
    let (_dir, fd_socket, conn) = connect().await?;
    let resp = request(&conn, 1, 1, &null_fds(1)?).await?;
    let slot = word(&resp, 1);
    assert!(has_slot(&fd_socket, slot));

    drop(conn);
    wait_for(|| !has_slot(&fd_socket, slot)).await?;
    Ok(())
}

#[tokio::test]
async fn messages_of_other_sizes_are_ignored() -> Result<()> {
    let (_dir, fd_socket, conn) = connect().await?;
    let resp = request(&conn, 1, 1, &null_fds(1)?).await?;
    let slot = word(&resp, 1);
    // Too short and too long, with a descriptor which must not end up in
    // a slot. An empty one does not end the connection either.
    send(&conn, &[1; 4], &null_fds(1)?).await?;
    send(&conn, &[1; 16], &null_fds(1)?).await?;
    send(&conn, &[], &[]).await?;

    let resp = request(&conn, 3, 1, &null_fds(1)?).await?;
    assert_eq!(word(&resp, 0), (3 << 32) | 1);
    assert_eq!(lock(&fd_socket.state).fds.len(), 2);
    assert!(has_slot(&fd_socket, slot));
    Ok(())
}

fn word(buf: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(buf[i * 8..(i + 1) * 8].try_into().expect("8 bytes"))
}

#[tokio::test]
async fn fds_round_trip() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("fd.sock");
    let fd_socket = Arc::new(FdSocket::default());
    assert_eq!(fd_socket.clone().start(path.clone()).await?, path);
    let conn = UnixSeqpacket::connect(&path).await?;

    let fds: Vec<OwnedFd> = vec![
        File::open("/dev/null")?.into(),
        File::open("/dev/null")?.into(),
    ];
    let resp = request(&conn, 7, 2, &fds).await?;
    assert_eq!(resp.len(), 3 * 8);
    assert_eq!(word(&resp, 0), (7 << 32) | 2);
    let slots = [word(&resp, 1), word(&resp, 2)];

    // A request which does not match the descriptors fails as a whole.
    let resp = request(&conn, 8, 3, &fds[..1]).await?;
    assert_eq!(word(&resp, 0), (8 << 32) | 0xffff_ffff);
    assert!(String::from_utf8_lossy(&resp[8..]).contains("expected 3 fds"));

    // Taking is all or nothing.
    assert!(fd_socket.take_all([slots[0], 999]).is_err());
    assert!(fd_socket.take_all([slots[0], slots[0]]).is_err());
    assert_eq!(fd_socket.take_all(slots)?.len(), 2);
    assert!(fd_socket.take_all([slots[0]]).is_err());
    assert!(fd_socket.take_all([])?.is_empty());

    // The most descriptors a message can carry all arrive.
    let fds = (0..MAX_FDS)
        .map(|_| Ok(File::open("/dev/null")?.into()))
        .collect::<Result<Vec<OwnedFd>>>()?;
    let resp = request(&conn, 9, MAX_FDS as u64, &fds).await?;
    assert_eq!(word(&resp, 0), (9 << 32) | MAX_FDS as u64);
    assert_eq!(resp.len(), (MAX_FDS + 1) * 8);
    Ok(())
}

#[test]
fn connection_holds_a_limited_number_of_fds() -> Result<()> {
    let fd_socket = Arc::new(FdSocket::default());
    let mut guard = ConnectionGuard {
        fd_socket: fd_socket.clone(),
        slots: Vec::new(),
    };
    let limits = Limits {
        per_connection: 3,
        total: 5,
    };
    let slots = Server::add_received_fds(&mut guard, null_fds(2)?, 2, limits)?;

    // Beyond the limit nothing gets added.
    let err = Server::add_received_fds(&mut guard, null_fds(2)?, 2, limits).unwrap_err();
    assert!(
        err.to_string().contains("holds 2 file descriptors"),
        "{err}"
    );
    assert_eq!(lock(&fd_socket.state).fds.len(), 2);

    // Descriptors which a request took do not count anymore.
    fd_socket.take_all(slots)?;
    Server::add_received_fds(&mut guard, null_fds(3)?, 3, limits)?;
    assert_eq!(guard.slots.len(), 3);

    // Another connection counts towards the total.
    let mut other = ConnectionGuard {
        fd_socket,
        slots: Vec::new(),
    };
    Server::add_received_fds(&mut other, null_fds(2)?, 2, limits)?;
    let err = Server::add_received_fds(&mut other, null_fds(1)?, 1, limits).unwrap_err();
    assert!(
        err.to_string()
            .contains("connections hold 5 file descriptors"),
        "{err}"
    );
    assert!(Limits::of_server().total > 0);
    Ok(())
}
