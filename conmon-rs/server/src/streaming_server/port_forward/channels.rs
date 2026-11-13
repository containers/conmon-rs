//! The channel protocol: a connection per port in the query, each with
//! a data and an error channel.

use super::{
    CHANNEL_BUFFER, CHANNEL_QUEUE, Early, MAX_CHANNEL_PORTS, OUTPUT_QUEUE, PORT_QUERY, PortForward,
    namespace::Namespace,
};
use crate::streaming_server::{
    budget::{Budget, Held},
    channel,
    idle::Activity,
    web::{self, ClientMessage, Message, WebSocket},
};
use anyhow::{Context, Result, ensure};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use std::{pin::pin, sync::Arc};
use tokio::{
    net::TcpStream,
    select,
    sync::mpsc,
    task::{self, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

/// Sends the messages of a port to the client.
#[derive(Clone)]
pub(super) struct ChannelOutput {
    pub(super) messages: mpsc::Sender<Message>,
    pub(super) base64: bool,
    pub(super) data: u8,
    pub(super) error: u8,
}

impl ChannelOutput {
    /// Send data on a channel, fails once the client went away.
    async fn send(&self, channel: u8, data: &[u8]) -> Result<()> {
        let message = channel::message(self.base64, channel, data);
        self.messages
            .send(message)
            .await
            .map_err(|_| anyhow::anyhow!("WebSocket closed"))
    }
}

impl PortForward {
    /// The ports a channel protocol request forwards, by its query. The
    /// `port` parameter may repeat and hold several ports separated by
    /// commas.
    pub(crate) fn query_ports(query: Option<&str>) -> Result<Vec<u16>> {
        let mut ports = vec![];
        for pair in query.unwrap_or_default().split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if Self::percent_decode(name) != PORT_QUERY {
                continue;
            }
            let value = Self::percent_decode(value);
            ensure!(
                !value.is_empty(),
                "query parameter {PORT_QUERY:?} cannot be empty"
            );
            for port in value.split(',') {
                let port: u16 = port
                    .parse()
                    .with_context(|| format!("unable to parse {port:?} as a port"))?;
                ensure!(port > 0, "port {port} must be > 0");
                ports.push(port);
            }
        }
        ensure!(
            !ports.is_empty(),
            "query parameter {PORT_QUERY:?} is required"
        );
        ensure!(
            ports.len() <= MAX_CHANNEL_PORTS,
            "at most {MAX_CHANNEL_PORTS} ports can be forwarded at once"
        );
        Ok(ports)
    }

    /// Decode a query component: a `+` is a space and `%XX` the byte of the
    /// two hex digits. Invalid escapes stay as they are, and bytes which are
    /// no UTF-8 become the replacement character.
    pub(super) fn percent_decode(s: &str) -> String {
        let hex = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            b'A'..=b'F' => Some(digit - b'A' + 10),
            _ => None,
        };
        let mut decoded = Vec::with_capacity(s.len());
        let mut rest = s.as_bytes();
        while let [byte, tail @ ..] = rest {
            rest = tail;
            decoded.push(match (byte, tail) {
                (b'%', [high, low, tail @ ..]) => match (hex(*high), hex(*low)) {
                    (Some(high), Some(low)) => {
                        rest = tail;
                        (high << 4) | low
                    }
                    _ => b'%',
                },
                (b'+', _) => b' ',
                (byte, _) => *byte,
            });
        }
        String::from_utf8_lossy(&decoded).into_owned()
    }

    /// Serve the channel protocol: forward a connection per port until all
    /// of them ended, the client went away or the WebSocket was idle.
    pub(super) async fn serve_channels(self, socket: WebSocket, ports: Vec<u16>, base64: bool) {
        let discard = socket.get_ref().discarder();
        let written = socket.get_ref().written();
        let (mut sink, mut messages) = socket.split();
        let (output_tx, mut output_rx) = mpsc::channel(OUTPUT_QUEUE);
        let activity = Activity::new();
        let too_large = CancellationToken::new();

        // On its own, as a port which does not take its input stops reading
        // the WebSocket.
        let idle = CancellationToken::new();
        // Ends the watch however the session ends.
        let _idle_guard = idle.clone().drop_guard();
        activity.watch(self.idle_timeout, idle.clone(), {
            let idle = idle.clone();
            move || {
                debug!("Closing idle port forward WebSocket");
                idle.cancel();
            }
        });

        let (writer_activity, writer_too_large) = (activity.clone(), too_large.clone());
        // The sink, once the close went out.
        let writer = task::spawn(async move {
            while let Some(message) = output_rx.recv().await {
                if sink.send(message).await.is_err() {
                    return None;
                }
                writer_activity.touch();
            }
            web::send_close(sink, writer_too_large.is_cancelled()).await
        });

        let namespace = Namespace::new(self.netns_path);
        let budget = Budget::new(CHANNEL_BUFFER);
        let mut inputs = Vec::with_capacity(ports.len());
        let mut forwards = JoinSet::new();
        for (i, port) in ports.into_iter().enumerate() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "at most MAX_CHANNEL_PORTS ports"
            )]
            let output = ChannelOutput {
                messages: output_tx.clone(),
                base64,
                data: (i * 2) as u8,
                error: (i * 2 + 1) as u8,
            };
            // The client learns the port of each channel from its first
            // message.
            let prefix = port.to_le_bytes();
            if output.send(output.data, &prefix).await.is_err()
                || output.send(output.error, &prefix).await.is_err()
            {
                return;
            }
            let (input_tx, input_rx) = mpsc::channel(CHANNEL_QUEUE);
            inputs.push(input_tx);
            forwards.spawn(Self::forward_channel(
                namespace.clone(),
                port,
                input_rx,
                output,
            ));
        }
        drop(output_tx);

        let mut end = web::End::Done;
        loop {
            select! {
                // Nothing gets dispatched after the idle timeout, and the
                // session ends with its last forward, also while the client
                // keeps sending.
                biased;
                () = idle.cancelled() => break,
                done = forwards.join_next() => if done.is_none() {
                    break;
                },
                message = web::next_message(&mut messages, &discard) => {
                    // The heartbeat of a client does not keep it from going
                    // idle, see [`web::next_message`].
                    let data = match message {
                        ClientMessage::Data(message) => message.into_data(),
                        // The client cannot be read anymore, it gets the
                        // close only. Like for Kubernetes, that ends the
                        // session, the error channels of the ports are not
                        // about it.
                        ClientMessage::TooLarge => {
                            too_large.cancel();
                            end = web::End::TooLarge;
                            break;
                        }
                        ClientMessage::Closed(_) => {
                            web::answer_close(&mut messages, &written).await;
                            end = web::End::Gone;
                            break;
                        }
                        ClientMessage::Gone => {
                            end = web::End::Gone;
                            break;
                        }
                    };
                    activity.touch();
                    Self::dispatch(&inputs, &budget, base64, data, &idle).await;
                    // A dispatch which ended with the idle timeout is no
                    // activity.
                    if idle.is_cancelled() {
                        break;
                    }
                    // A port which takes its input slowly keeps the WebSocket
                    // active.
                    activity.touch();
                }
            }
        }

        // The ports still forwarded lose their client.
        forwards.abort_all();
        while forwards.join_next().await.is_some() {}
        // Like the error of a port which failed before its input came, the
        // client has to get what the server sent before.
        web::end(writer, &written, messages, end).await;
    }

    /// Hand a message of the client to the connection of its channel. Only
    /// the data channels, the even ones, take input. Waits while a connected
    /// port does not take its input or the ports hold all of the budget,
    /// which stops reading the WebSocket like the reference implementation
    /// does, the protocol has no flow control. Connecting ports take it, see
    /// [`Self::drain_until`], and the idle timeout still ends the wait.
    pub(super) async fn dispatch(
        inputs: &[mpsc::Sender<Held>],
        budget: &Budget,
        base64: bool,
        message: Bytes,
        idle: &CancellationToken,
    ) {
        let Some((channel, data)) = channel::decode(base64, message) else {
            return;
        };
        let Some(input) = inputs
            .get(usize::from(channel / 2))
            .filter(|_| channel % 2 == 0)
        else {
            trace!("Dropping port forward message for channel {channel}");
            return;
        };
        if !data.is_empty() {
            select! {
                // A connection which ended does not take input anymore.
                _ = async { input.send(budget.hold(data).await).await } => {}
                // Also while waiting for the budget.
                () = input.closed() => {}
                () = idle.cancelled() => {}
            }
        }
    }

    /// Forward the connection of a port, the error channel reports a failure.
    pub(super) async fn forward_channel(
        namespace: Arc<Namespace>,
        port: u16,
        mut input: mpsc::Receiver<Held>,
        output: ChannelOutput,
    ) {
        debug!("Forwarding port {port} over WebSocket");
        let result = match Self::establish(&namespace, port, &mut input).await {
            Ok((connection, early)) => {
                Self::copy_channel(early, &mut input, &output, connection).await
            }
            Err(e) => Err(e),
        };
        // What the client sent gives its budget back, and the WebSocket gets
        // read on while the error waits for a client which does not read.
        drop(input);
        if let Err(e) = result {
            let message = Self::forward_error(port, &e);
            debug!("Port forward over WebSocket: {message}");
            let _ = output.send(output.error, message.as_bytes()).await;
        }
    }

    /// Copy between the channels of a port and its connection in the pod,
    /// until the pod closed the connection, writing to it failed, see
    /// [`Self::upstream_failed`], or the client went away. The
    /// protocol has no way to close a channel, the input after the pod closed
    /// the connection does not matter.
    async fn copy_channel(
        early: Early,
        input: &mut mpsc::Receiver<Held>,
        output: &ChannelOutput,
        connection: TcpStream,
    ) -> Result<()> {
        let (mut from_container, mut to_container) = connection.into_split();
        let upstream = Self::write_input(early, input, &mut to_container);

        let downstream = async {
            let mut buf = BytesMut::new();
            while let Some(data) = Self::read_pod(&mut from_container, &mut buf).await? {
                if output.send(output.data, &data).await.is_err() {
                    // The client went away.
                    return Ok(());
                }
            }
            anyhow::Ok(())
        };

        let mut downstream = pin!(downstream);
        select! {
            result = &mut downstream => result,
            result = upstream => match result {
                Ok(()) => Ok(()),
                Err(e) => Self::upstream_failed(e, downstream).await,
            },
        }
    }
}
