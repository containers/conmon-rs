//! Sessions which go idle, and their final status.

use super::*;

/// On the paused clock, which gets to the timeouts at once and does not
/// stall like the real one on a loaded machine.
#[tokio::test(start_paused = true)]
async fn idle_websocket_sessions_end() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_duplex(true, true, Some(timeout)).await?;
            let attach = io.attach().clone();

            // Stdin keeps the session going.
            for _ in 0..20 {
                time::sleep(timeout / 10).await;
                stream
                    .write_all(&client_frame(0x2, &[STDIN_BYTE, b'x']))
                    .await?;
            }
            assert!(!attach.stdin_closed().is_cancelled());

            // A heartbeat of pings and pongs does not, the session ends
            // like for a client which went away, which closes the stdin
            // once and the connection.
            let pongs = async {
                for opcode in [0x9, 0xa].into_iter().cycle() {
                    time::sleep(timeout / 5).await;
                    if stream.write_all(&client_frame(opcode, &[])).await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                result = time::timeout(4 * timeout, attach.stdin_closed().cancelled()) => result?,
                () = pongs => bail!("connection ended before the stdin got closed"),
            }
            // The client learns about the timeout before the close, which
            // it answers.
            let mut output = vec![];
            loop {
                match server_frame(&mut stream).await? {
                    (0x8, _) => break,
                    (_, payload) => output.extend(payload),
                }
            }
            let output = String::from_utf8_lossy(&output);
            assert!(output.contains("idle timeout"), "{output}");
            stream.write_all(&client_frame(0x8, &[])).await?;
            let mut rest = vec![];
            time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest)).await??;
            Ok(())
        })
        .await
}

#[tokio::test]
async fn stdin_backed_up_behind_the_container_goes_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_with(false, true, true, Some(timeout)).await?;
            let attach = io.attach().clone();

            // Stdin the container never reads, until the client is not
            // read anymore. The session ends like for a client which went
            // away, although the client still sends.
            let mut frame = vec![STDIN_BYTE];
            frame.resize(60_000, b'x');
            let frame = client_frame(0x2, &frame);
            let send = async { while stream.write_all(&frame).await.is_ok() {} };
            // The stdin gets closed before the connection ends.
            tokio::select! {
                biased;
                result = time::timeout(10 * timeout, attach.stdin_closed().cancelled()) => {
                    result.context("the session did not end")?;
                }
                () = send => bail!("connection ended before the stdin got closed"),
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn clients_which_stop_reading_go_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_secs(1);
            let (io, _server, stream) =
                attach_session_with(false, true, true, Some(timeout)).await?;
            let attach = io.attach().clone();

            // The container writes output the client never reads.
            let output = {
                let attach = attach.clone();
                task::spawn(async move {
                    let data = Bytes::from(vec![b'o'; 64 * 1024]);
                    loop {
                        attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                        time::sleep(Duration::from_millis(5)).await;
                    }
                })
            };
            let ended = time::timeout(10 * timeout, attach.stdin_closed().cancelled()).await;
            output.abort();
            drop(stream);
            ended.context("the session of a stuck client did not end")?;
            Ok(())
        })
        .await
}

#[test]
fn only_one_final_status() -> Result<()> {
    let (log, _subscriber) = Log::capture();
    let idle = || {
        Err(anyhow!(
            "no activity on the connection for the idle timeout"
        ))
    };
    // A status the loop decided on goes out also after the idle timeout,
    // a failure never turns into a success.
    assert_eq!(
        StreamingServer::final_status(idle(), Some(Status::exited(1)), Operation::Exec),
        Some(Status::ExitCode(1))
    );
    assert_eq!(
        StreamingServer::final_status(Ok(()), Some(Status::exited(1)), Operation::Exec),
        Some(Status::ExitCode(1))
    );
    assert!(matches!(
        StreamingServer::final_status(idle(), None, Operation::Exec),
        Some(Status::Error(e)) if e.contains("idle timeout")
    ));
    // Only that failure gets logged, as a warning: mostly the client
    // caused it. The span of the session names the container.
    let logged = log.contents()?;
    assert_eq!(
        logged.trim(),
        "WARN Unable to run exec: no activity on the connection for the idle timeout"
    );
    // A client which went away gets none.
    assert_eq!(
        StreamingServer::final_status(Ok(()), None, Operation::Attach),
        None
    );
    Ok(())
}

#[tokio::test]
async fn idle_timeouts_of_stuck_websocket_sessions_send_one_status() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut stream) =
                attach_session_with(false, false, true, Some(timeout)).await?;
            let attach = io.attach().clone();

            // More output than the connection holds while the client does
            // not read, then the container exits, around the timeout.
            let data = Bytes::from(vec![b'o'; 64 * 1024]);
            for _ in 0..256 {
                attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
            }
            time::sleep(timeout).await;
            attach.write(IOMessage::Done);
            time::sleep(2 * timeout).await;

            // Exactly one status, which is no success.
            let statuses = time::timeout(Duration::from_secs(10), async {
                let mut statuses = vec![];
                loop {
                    let (opcode, payload) = server_frame(&mut stream).await?;
                    match opcode {
                        8 => return anyhow::Ok(statuses),
                        2 if payload.first() == Some(&STREAM_ERR_BYTE) => {
                            statuses.push(String::from_utf8_lossy(&payload[1..]).into_owned());
                        }
                        _ => {}
                    }
                }
            })
            .await??;
            assert_eq!(statuses.len(), 1, "{statuses:?}");
            assert!(!statuses[0].contains("\"Success\""), "{statuses:?}");
            Ok(())
        })
        .await
}

/// On the paused clock, like [`idle_websocket_sessions_end`].
#[tokio::test(start_paused = true)]
async fn idle_spdy_sessions_end() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, mut client) =
                spdy_attach_duplex(true, Some(timeout), Version::V4, 64 * 1024).await?;
            let attach = io.attach().clone();

            // Stdin keeps the session going.
            for _ in 0..20 {
                time::sleep(timeout / 10).await;
                client
                    .send(Frame::Data {
                        stream_id: 3,
                        fin: false,
                        data: Bytes::from_static(b"x"),
                    })
                    .await?;
            }
            assert!(!attach.stdin_closed().is_cancelled());

            // The pings of the server and their echoes do not. The client
            // learns about the timeout on the error stream, it must not
            // take the end for a success.
            let end = time::timeout(4 * timeout, async {
                let mut status = vec![];
                loop {
                    match client.frame().await? {
                        Some(Frame::Ping { id }) => client.send(Frame::Ping { id }).await?,
                        Some(Frame::Data {
                            stream_id: 1, data, ..
                        }) => status.extend_from_slice(&data),
                        Some(_) => {}
                        None => return anyhow::Ok(status),
                    }
                }
            });
            let status = String::from_utf8(end.await??)?;
            assert!(status.contains("\"status\":\"Failure\""), "{status}");
            assert!(status.contains("idle timeout"), "{status}");
            assert!(attach.stdin_closed().is_cancelled());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn idle_timeouts_of_stuck_spdy_sessions_send_one_status() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, _server, mut client) = spdy_attach_with(false, true, Some(timeout)).await?;
            let attach = io.attach().clone();

            // More output than the connection holds while the client does
            // not read, then the container exits, around the timeout.
            let data = Bytes::from(vec![b'o'; 64 * 1024]);
            for _ in 0..256 {
                attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
            }
            time::sleep(timeout).await;
            attach.write(IOMessage::Done);
            time::sleep(2 * timeout).await;

            // Exactly one status on the error stream, which is no success.
            let statuses = time::timeout(Duration::from_secs(10), async {
                let mut statuses = vec![];
                loop {
                    match client.frame().await? {
                        Some(Frame::Data {
                            stream_id: 1, data, ..
                        }) if !data.is_empty() => {
                            statuses.push(String::from_utf8_lossy(&data).into_owned());
                        }
                        Some(_) => {}
                        None => return anyhow::Ok(statuses),
                    }
                }
            })
            .await??;
            assert_eq!(statuses.len(), 1, "{statuses:?}");
            assert!(statuses[0].contains("\"Failure\""), "{statuses:?}");
            Ok(())
        })
        .await
}

/// A client which floods pings and never reads. Its pings count as
/// activity, but their answers back up in the bounded queues of the server,
/// which then stops reading the client, so the session still goes idle. A
/// v1 client, whose stdin the server closes at the start. Over TCP, the
/// socket buffers take hundreds of thousands of pings before the queues fill,
/// which is too slow under qemu. A 4 KiB connection in memory fills after a
/// few hundred.
#[tokio::test]
async fn clients_which_flood_pings_without_reading_go_idle() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let timeout = Duration::from_millis(500);
            let (io, mut client) =
                spdy_attach_duplex(true, Some(timeout), Version::V1, 4096).await?;
            let attach = io.attach().clone();
            // Until the server ended the connection.
            let flood = async {
                let mut id: u32 = 1;
                while client.send(Frame::Ping { id }).await.is_ok() {
                    id = id.wrapping_add(2);
                }
                std::future::pending::<()>().await;
            };
            tokio::select! {
                result = time::timeout(10 * timeout, attach.stdin_closed().cancelled()) => {
                    result.context("the session of a flooding client did not end")
                }
                () = flood => unreachable!(),
            }
        })
        .await
}
