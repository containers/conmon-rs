//! Messages and sessions beyond the size limits.

use super::*;

#[tokio::test]
async fn messages_larger_than_the_limit_fail_the_session() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (_io, _server, stream) = attach_session(true, false).await?;
            let (mut reader, writer) = send_too_large(stream, MAX_MESSAGE_SIZE);

            // The client learns why, from the status and the close.
            let (opcode, status) = server_frame(&mut reader).await?;
            assert_eq!((opcode, status.first()), (2, Some(&STREAM_ERR_BYTE)));
            let status = String::from_utf8_lossy(&status[1..]);
            assert!(status.contains("\"Failure\""), "{status}");
            assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
            too_large_close(reader, writer).await
        })
        .await
}

#[tokio::test]
async fn too_large_messages_of_clients_which_do_not_read_fail_the_session() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, stream) = attach_session(true, false).await?;
            let attach = io.attach().clone();
            // A small buffer, so that the message does not fit into it.
            setsockopt(&stream, sockopt::SndBuf, &4096)?;

            // The container keeps writing, until the session waits for
            // the client to read. The client then writes all of its
            // message before it reads, like a client with a single
            // thread.
            let output = {
                let attach = attach.clone();
                task::spawn(async move {
                    let data = Bytes::from(vec![b'o'; 64 * 1024]);
                    loop {
                        attach.write(IOMessage::Data(data.clone(), Pipe::StdOut));
                        time::sleep(Duration::from_millis(1)).await;
                    }
                })
            };
            time::sleep(Duration::from_secs(1)).await;
            let (mut reader, writer) = send_too_large(stream, MAX_MESSAGE_SIZE);
            let written = wait_for(|| writer.is_finished()).await;
            output.abort();
            written.context("the server does not take the rest of the message")?;

            // The output before, then the status and the close.
            let mut status = None;
            loop {
                match server_frame(&mut reader).await? {
                    (8, payload) => {
                        assert_eq!(payload[..2], u16::from(CloseCode::Size).to_be_bytes());
                        break;
                    }
                    (2, payload) if payload.first() == Some(&STREAM_ERR_BYTE) => {
                        status = Some(String::from_utf8_lossy(&payload[1..]).into_owned());
                    }
                    _ => {}
                }
            }
            let status = status.context("no status")?;
            assert!(status.contains(MESSAGE_TOO_LARGE), "{status}");
            too_large_end(reader, writer).await
        })
        .await
}

#[tokio::test(start_paused = true)]
async fn too_large_sessions_end_without_the_idle_timeout() -> Result<()> {
    let idle_timeout = Some(Duration::from_secs(3600));
    let (mut output, client) = spdy_output(idle_timeout).await?;

    // The client does not read, the output backs up.
    let data = Bytes::from(vec![0; 32 * 1024]);
    let backed_up = async { while output.data(STDOUT_BYTE, &data).await.is_ok() {} };
    assert!(
        time::timeout(Duration::from_secs(1), backed_up)
            .await
            .is_err()
    );

    // Then it sends a message which is too large, which fails the session.
    let (_io, child, _token) = attach_child(false)?;
    let session = Session::Attach(attach_config(child, false, false, idle_timeout));
    let (client_rx, _senders) = ClientRx::idle();
    client_rx.too_large.cancel();

    // The status and the close do not wait for the idle timeout.
    let start = time::Instant::now();
    let output = StreamingServer::write_task(output, client_rx, session, Activity::new()).await?;
    assert!(output.is_none());
    assert_eq!(start.elapsed(), STALL_TIMEOUT);
    drop(client);
    Ok(())
}
