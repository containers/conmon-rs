//! Stdin of the client which reaches the container, also around the close.

use super::*;

#[tokio::test]
async fn stdin_before_close_reaches_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(true, false).await?;

            // More stdin than the channels on the way hold, right before
            // the close, in one write.
            let mut frames = vec![];
            for i in 0..50u8 {
                frames.extend(client_frame(0x2, &[STDIN_BYTE, i]));
            }
            frames.extend(client_frame(0x8, &1000u16.to_be_bytes()));
            stream.write_all(&frames).await?;

            // The container writes output meanwhile, which cannot be sent
            // after the close anymore. The stdin which waits gets merged.
            let attach = io.attach().clone();
            let mut stdin = vec![];
            while stdin.len() < 50 {
                attach.write(IOMessage::Data(Bytes::from_static(b"out"), Pipe::StdOut));
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                stdin.extend_from_slice(&data);
            }
            assert_eq!(stdin, (0..50u8).collect::<Vec<_>>());
            Ok(())
        })
        .await
}

#[tokio::test]
async fn messages_larger_than_the_pending_stdin_reach_the_container() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (io, _server, mut stream) = attach_session(true, false).await?;

            // Like the Python client, which sends 1 MiB and the channel byte
            // per message for `k8s_cp` of Ansible.
            let mut message = vec![STDIN_BYTE];
            message.resize(PENDING_STDIN_MAX_BYTES + 1, b'x');
            stream.write_all(&client_frame(0x2, &message)).await?;

            let attach = io.attach().clone();
            let mut stdin = 0;
            while stdin < PENDING_STDIN_MAX_BYTES {
                let data = time::timeout(Duration::from_secs(5), attach.read()).await??;
                assert!(data.iter().all(|b| *b == b'x'));
                stdin += data.len();
            }
            assert_eq!(stdin, PENDING_STDIN_MAX_BYTES);
            Ok(())
        })
        .await
}

/// A client which sends the events, then waits.
struct Events(VecDeque<ClientEvent>);

impl ClientInput for Events {
    async fn next(&mut self) -> Option<ClientEvent> {
        match self.0.pop_front() {
            Some(event) => Some(event),
            None => std::future::pending().await,
        }
    }
}

/// Stdin in messages of 32 KiB, each of its own byte, and a clean close.
fn stdin_and_close(messages: u8) -> (Events, Vec<u8>) {
    let stdin: Vec<_> = (0..messages).map(|i| vec![i; 32 * 1024]).collect();
    let events = stdin
        .iter()
        .map(|data| ClientEvent::Stdin(Stdin::Data(Bytes::from(data.clone()))))
        .chain([ClientEvent::Closed])
        .collect();
    (Events(events), stdin.concat())
}

/// A client which closes right after its last stdin, like the Python client
/// with `k8s_cp` of Ansible, while the container takes it slowly: all of it
/// reaches the container, the end of the session waits for it.
#[tokio::test(start_paused = true)]
async fn stdin_before_close_reaches_slow_containers() -> Result<()> {
    let (io, child, _token) = attach_child(false)?;
    let session = Session::Attach(attach_config(child, true, false, None));
    let (output, _client) = spdy_output(None).await?;
    let (input, expected) = stdin_and_close(64);

    // Far more than the server holds, the container takes a part well
    // within the timeout, but all of it only long after.
    let attach = io.attach().clone();
    let reader = task::spawn(async move {
        let mut stdin = vec![];
        loop {
            time::sleep(STDIN_FLUSH_TIMEOUT / 10).await;
            match time::timeout(STDIN_FLUSH_TIMEOUT, attach.read()).await {
                Ok(data) => stdin.extend_from_slice(&data?),
                Err(_) => return anyhow::Ok(stdin),
            }
        }
    });
    StreamingServer::run_session(output, input, session, Activity::new()).await;
    let stdin = reader.await??;
    assert!(
        stdin == expected,
        "got {} of {} bytes",
        stdin.len(),
        expected.len()
    );
    Ok(())
}

/// A container which stops taking stdin after a clean close of the client
/// holds up the end of the session for [`STDIN_FLUSH_TIMEOUT`] after it took
/// the last part.
#[tokio::test(start_paused = true)]
async fn stdin_before_close_waits_for_stalled_containers_in_bounds() -> Result<()> {
    let (io, child, _token) = attach_child(false)?;
    let session = Session::Attach(attach_config(child, true, false, None));
    let (output, _client) = spdy_output(None).await?;
    // More than the channels towards the container hold, less than the
    // pending stdin, so that the server reads the close.
    let (input, _) = stdin_and_close(40);

    // Each part within the timeout, both together take longer.
    let pause = STDIN_FLUSH_TIMEOUT * 3 / 5;
    let attach = io.attach().clone();
    let reader = task::spawn(async move {
        for _ in 0..2 {
            time::sleep(pause).await;
            attach.read().await?;
        }
        anyhow::Ok(())
    });
    let start = time::Instant::now();
    StreamingServer::run_session(output, input, session, Activity::new()).await;
    assert_eq!(start.elapsed(), 2 * pause + STDIN_FLUSH_TIMEOUT);
    reader.await??;
    Ok(())
}

/// A client which closes after stdin which the container does not take gets
/// the answer to its close right away, also when the session gets stopped
/// after [`STDIN_FLUSH_TIMEOUT`] then. On a paused clock, which only the
/// timers of the session advance, so that slow test runs do not matter.
#[tokio::test(start_paused = true)]
async fn closes_after_stdin_stalled_containers_get_answered() -> Result<()> {
    LocalSet::new()
        .run_until(async {
            let (_io, _server, mut stream) = attach_session_duplex(true, false, None).await?;
            // Like for the SPDY sessions above, the server still reads the
            // close.
            let mut stdin = vec![STDIN_BYTE];
            stdin.resize(32 * 1024 + 1, b'x');
            let mut frames = vec![];
            for _ in 0..40 {
                frames.extend(client_frame(0x2, &stdin));
            }
            let normal = u16::from(CloseCode::Normal).to_be_bytes();
            frames.extend(client_frame(0x8, &normal));
            let start = time::Instant::now();
            stream.write_all(&frames).await?;

            // Before the flush gives up. Pings of the server may come first.
            let answer = loop {
                let frame = server_frame(&mut stream).await?;
                if frame.0 != 0x9 {
                    break frame;
                }
            };
            assert_eq!(answer, (0x8, normal.to_vec()));
            assert!(start.elapsed() < STDIN_FLUSH_TIMEOUT / 2);
            let mut rest = vec![];
            time::timeout(
                STDIN_FLUSH_TIMEOUT + Duration::from_secs(5),
                stream.read_to_end(&mut rest),
            )
            .await??;
            assert!(rest.is_empty(), "{rest:?}");
            assert!(start.elapsed() >= STDIN_FLUSH_TIMEOUT);
            Ok(())
        })
        .await
}
