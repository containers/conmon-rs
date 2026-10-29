//! Attach sessions, which connect their client to the container process.

use super::{
    StreamingServer,
    session::{ClientRx, InputEvent, SessionInput},
};
use crate::{
    attach::SharedContainerAttach,
    child_reaper::ReapableChild,
    container_io::{Drain, Message as IOMessage, Pipe},
    streaming_server::remotecommand::{
        MESSAGE_TOO_LARGE, Output, STDERR_BYTE, STDOUT_BYTE, Status,
    },
};
use anyhow::{Result, bail};
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

#[derive(Debug)]
/// Required attach session data.
pub struct AttachSession {
    pub container_id: Box<str>,
    pub child: ReapableChild,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub stop_after_stdin_eof: bool,
    pub idle_timeout: Option<Duration>,
}

impl StreamingServer {
    /// The attach specific read/write loop. `status` gets the success once
    /// the session ended, unless the client went away, which goes out after
    /// the loop.
    pub(super) async fn attach_loop(
        session: AttachSession,
        sender: &mut Output,
        client: ClientRx,
        status: &mut Option<Status>,
    ) -> Result<()> {
        // Extract values we need for the I/O loop
        let stdin_enabled = session.stdin;
        let stdout_enabled = session.stdout;
        let stderr_enabled = session.stderr;
        let stop_after_stdin_eof = session.stop_after_stdin_eof;
        let io = session.child.io().clone();
        let token = session.child.token().clone();

        // Explicitly drop session to free the ReapableChild before entering the I/O loop
        drop(session);

        // Subscribe to the attach broadcast rather than the stdio channels. The
        // broadcast only carries output produced while someone is attached,
        // whereas the stdio channels replay buffered history and are closed
        // for created containers anyway.
        let attach = io.attach().await;
        let terminal = attach.is_terminal();
        // Without stdin, there is none to wait for.
        let mut input = SessionInput::new(io, attach.clone(), client, stdin_enabled);

        // With stdin once, the end of the session closes the container stdin,
        // also if the client went away without closing it, like the first
        // session ending does with StdinOnce in Kubernetes.
        let mut teardown = AttachTeardown {
            attach: attach.clone(),
            token: token.clone(),
            close_stdin: stdin_enabled && stop_after_stdin_eof,
        };

        // A single `Done` follows once all pipes ended. It may have been sent
        // before subscribing, then only forward what is still queued.
        let (mut attach_rx, mut done) = attach.subscribe();
        if done {
            while let Ok(message) = attach_rx.try_recv() {
                Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
            }
        }

        // Until nothing is left to forward in either direction.
        while !(done && !input.stdin_open) {
            tokio::select! {
                res = attach_rx.recv(), if !done => match res {
                    Ok(IOMessage::Done) => done = true,
                    Ok(message) => {
                        Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    Err(RecvError::Lagged(n)) => warn!("Attach client too slow, skipped {n} messages"),
                    Err(RecvError::Closed) => {
                        debug!("Exiting streaming attach because container I/O is gone");
                        break
                    }
                },

                event = input.next() => match event {
                    InputEvent::Resize(data) => input.resize(&data).await,
                    InputEvent::Ping => sender.ping().await?,
                    // Without a terminal and stdin once, that ends the
                    // session, like for CRI-O with conmon. Otherwise keep
                    // forwarding the output until it ends, and with stdin
                    // once close the container stdin.
                    InputEvent::StdinClosed if !terminal && !stop_after_stdin_eof => break,
                    InputEvent::StdinClosed => {
                        if stop_after_stdin_eof {
                            teardown.close_stdin = false;
                            input.close_container_stdin().await;
                        }
                    }
                    // The client closed the connection, everything it sent
                    // reached the container. Nobody gets a status anymore.
                    InputEvent::Ended => return Ok(()),
                    InputEvent::TooLarge => bail!(MESSAGE_TOO_LARGE),
                },

                () = token.cancelled() => {
                    debug!("Exiting streaming attach because token cancelled");
                    if !done {
                        Self::drain_attach(&mut attach_rx, stdout_enabled, stderr_enabled, sender).await?;
                    }
                    break
                }
            }
        }

        *status = Some(Status::Success);
        Ok(())
    }

    /// Forward a single output message to the client.
    async fn forward_output(
        message: IOMessage,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut Output,
    ) -> Result<()> {
        match message {
            IOMessage::Data(data, Pipe::StdOut) if stdout_enabled => {
                sender.data(STDOUT_BYTE, &data).await
            }
            IOMessage::Data(data, Pipe::StdErr) if stderr_enabled => {
                sender.data(STDERR_BYTE, &data).await
            }
            IOMessage::Data(..) | IOMessage::Done => Ok(()),
        }
    }

    /// Forward the output which is still queued or arrives shortly after the
    /// container exited, until the output ended or the [`Drain`] gives up.
    async fn drain_attach(
        attach_rx: &mut broadcast::Receiver<IOMessage>,
        stdout_enabled: bool,
        stderr_enabled: bool,
        sender: &mut Output,
    ) -> Result<()> {
        let mut drain = Drain::default();
        while let Some(message) = drain.recv_output(attach_rx, true).await {
            Self::forward_output(message, stdout_enabled, stderr_enabled, sender).await?;
        }
        Ok(())
    }
}

/// Closes the container stdin when an attach session with stdin once ends
/// while the container runs, also if the client went away without closing
/// its stdin and the loop got aborted.
struct AttachTeardown {
    attach: SharedContainerAttach,

    /// Cancelled once the container exited.
    token: CancellationToken,

    /// Whether the container stdin still has to be closed.
    close_stdin: bool,
}

impl Drop for AttachTeardown {
    fn drop(&mut self) {
        if self.close_stdin && !self.token.is_cancelled() {
            debug!("Closing the container stdin after the end of the attach session");
            self.attach.close_container_stdin();
        }
    }
}

#[cfg(test)]
mod tests;
