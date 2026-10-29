//! The SPDY protocol: the streams of a request pair up by their headers,
//! then the request forwards one connection.

use super::{
    CLOSE_GRACE_PERIOD, Early, PORT_HEADER, PortForward, REQUEST_ID_HEADER, STREAM_TYPE_DATA,
    STREAM_TYPE_ERROR, namespace::Namespace,
};
use crate::streaming_server::{
    spdy::{Connection, Headers, Stream, StreamReceiver, StreamSender},
    stream_protocol::{ClientStr, STREAM_CREATION_TIMEOUT, STREAM_TYPE_HEADER},
};
use anyhow::{Context, Result, bail, ensure};
use bytes::BytesMut;
use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::HashMap,
    future::Future,
    pin::{Pin, pin},
    sync::Arc,
};
use tokio::{io::AsyncWriteExt, net::TcpStream, select, time};
use tokio_util::{
    sync::{CancellationToken, DropGuard},
    time::{DelayQueue, delay_queue::Key},
};
use tracing::debug;

/// The streams of a request, until both arrived.
pub(super) struct PendingRequest {
    data: Option<Waiting>,
    error: Option<Waiting>,

    /// The deadline of the request in [`Pending::deadlines`].
    deadline: Key,
}

/// A stream which waits for the other one of its request. Its reset gets
/// watched until it stops waiting, see [`Pending::resets`].
struct Waiting {
    stream: Stream,
    _watch: DropGuard,
}

impl Waiting {
    fn is_reset(&self) -> bool {
        self.stream.is_reset()
    }
}

/// The watch of a waiting stream, which names its request once the client
/// reset it, nothing once the stream stopped waiting.
type ResetWatch = Pin<Box<dyn Future<Output = Option<Arc<str>>> + Send>>;

/// The requests waiting for their second stream, with their deadlines and
/// the watches of their streams. All are bounded by the streams of a
/// connection.
pub(super) struct Pending {
    pub(super) requests: HashMap<Arc<str>, PendingRequest>,
    pub(super) deadlines: DelayQueue<Arc<str>>,

    /// The watches of the waiting streams, which let go of a stream once
    /// the client reset it. Otherwise the streams of a client which opens
    /// and resets them would count against the most streams of its
    /// connection until their deadline, as no new stream which could free
    /// them gets accepted once there is no room left.
    pub(super) resets: FuturesUnordered<ResetWatch>,
}

impl Pending {
    pub(super) fn new() -> Self {
        Self {
            requests: HashMap::new(),
            deadlines: DelayQueue::new(),
            resets: FuturesUnordered::new(),
        }
    }

    /// A stream of a request which waits for the other one, and the watch
    /// for its reset, see [`Self::resets`].
    fn wait(request_id: &Arc<str>, stream: Stream) -> (Waiting, ResetWatch) {
        let waiting = CancellationToken::new();
        let (reset, stopped) = (stream.reset_watch(), waiting.clone());
        let request_id = request_id.clone();
        let watch = Box::pin(async move {
            select! {
                biased;
                () = stopped.cancelled() => None,
                () = reset => Some(request_id),
            }
        });
        let stream = Waiting {
            stream,
            _watch: waiting.drop_guard(),
        };
        (stream, watch)
    }

    /// The request, created with its deadline if it is new.
    pub(super) fn get_or_insert(&mut self, request_id: &Arc<str>) -> &mut PendingRequest {
        let Self {
            requests,
            deadlines,
            ..
        } = self;
        requests
            .entry(request_id.clone())
            .or_insert_with(|| PendingRequest {
                data: None,
                error: None,
                deadline: deadlines.insert(request_id.clone(), STREAM_CREATION_TIMEOUT),
            })
    }

    /// Remove a request, with its deadline.
    pub(super) fn remove(&mut self, request_id: &str) -> Option<PendingRequest> {
        let request = self.requests.remove(request_id)?;
        self.deadlines.remove(&request.deadline);
        Some(request)
    }

    /// Let go of the streams of a request which the client reset, and of
    /// the request once it has no stream left.
    pub(super) fn remove_reset(&mut self, request_id: &str) {
        let Some(request) = self.requests.get_mut(request_id) else {
            return;
        };
        request.data.take_if(|stream| stream.is_reset());
        request.error.take_if(|stream| stream.is_reset());
        if request.data.is_none() && request.error.is_none() {
            self.remove(request_id);
        }
    }
}

impl PortForward {
    /// Serve port forward requests on the upgraded connection until it ends.
    pub async fn serve_spdy<T>(self, io: T)
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let mut connection =
            Connection::serve(io, self.idle_timeout, self.ping_interval, Self::accept);
        let namespace = Namespace::new(self.netns_path);
        let mut pending = Pending::new();

        loop {
            select! {
                Some(reset) = pending.resets.next() => if let Some(request_id) = reset {
                    pending.remove_reset(&request_id);
                },
                stream = connection.accept() => match stream {
                    Some(stream) => Self::add(&mut pending, stream, &namespace),
                    None => break,
                },
                Some(expired) = pending.deadlines.next() => {
                    Self::expire(&mut pending, expired.into_inner());
                }
            }
        }
        debug!("Port forward connection ended");
    }

    /// Whether a stream is part of a valid request, by its headers.
    pub(super) fn accept(headers: &Headers) -> Result<()> {
        Self::port(headers)?;
        match headers.get(STREAM_TYPE_HEADER) {
            Some(STREAM_TYPE_DATA | STREAM_TYPE_ERROR) => Ok(()),
            Some(typ) => bail!("invalid stream type {:?}", ClientStr(typ)),
            None => bail!("{STREAM_TYPE_HEADER:?} header is required"),
        }
    }

    fn port(headers: &Headers) -> Result<u16> {
        let port = headers
            .get(PORT_HEADER)
            .with_context(|| format!("{PORT_HEADER:?} header is required"))?;
        let port: u16 = port
            .parse()
            .with_context(|| format!("unable to parse {:?} as a port", ClientStr(port)))?;
        ensure!(port > 0, "port {port} must be > 0");
        Ok(port)
    }

    fn is_error_stream(stream: &Stream) -> bool {
        stream.headers().get(STREAM_TYPE_HEADER) == Some(STREAM_TYPE_ERROR)
    }

    /// The request a stream belongs to. Old clients send no ID, their
    /// requests use the IDs of the error streams, the data streams follow
    /// them.
    fn request_id(stream: &Stream) -> Arc<str> {
        match stream.headers().get(REQUEST_ID_HEADER) {
            Some(id) if !id.is_empty() => id.into(),
            _ if Self::is_error_stream(stream) => stream.id().to_string().into(),
            _ => (i64::from(stream.id()) - 2).to_string().into(),
        }
    }

    /// Add a stream to its request, which starts once both streams arrived.
    pub(super) fn add(pending: &mut Pending, stream: Stream, namespace: &Arc<Namespace>) {
        let request_id = Self::request_id(&stream);
        let is_error = Self::is_error_stream(&stream);
        let (stream, watch) = Pending::wait(&request_id, stream);
        let request = pending.get_or_insert(&request_id);
        // A stream the client reset does not count, also before the reset
        // got handled.
        request.data.take_if(|stream| stream.is_reset());
        request.error.take_if(|stream| stream.is_reset());
        let slot = if is_error {
            &mut request.error
        } else {
            &mut request.data
        };
        if slot.is_some() {
            // Dropping it resets it.
            debug!(
                "Rejecting stream {} of port forward request {:?}, the request has one of its type already",
                stream.stream.id(),
                ClientStr(&request_id)
            );
            return;
        }
        *slot = Some(stream);

        if request.data.is_none() || request.error.is_none() {
            pending.resets.push(watch);
            return;
        }
        if let Some(PendingRequest {
            data: Some(data),
            error: Some(error),
            ..
        }) = pending.remove(&request_id)
        {
            tokio::spawn(Self::forward(
                request_id,
                data.stream,
                error.stream,
                namespace.clone(),
            ));
        }
    }

    /// Fail a request whose second stream did not arrive in time.
    fn expire(pending: &mut Pending, request_id: Arc<str>) {
        let Some(mut request) = pending.requests.remove(&request_id) else {
            return;
        };
        debug!(
            "Port forward request {:?} timed out waiting for streams",
            ClientStr(&request_id)
        );
        if let Some((_, mut error)) = request.error.take().map(|error| error.stream.split()) {
            let message = format!("request {request_id}: timed out waiting for streams");
            tokio::spawn(async move {
                let _ = error.send(message.into()).await;
                let _ = error.close().await;
            });
        }
    }

    /// Forward a single connection, the error stream reports a failure.
    async fn forward(request_id: Arc<str>, data: Stream, error: Stream, namespace: Arc<Namespace>) {
        let port = Self::port(data.headers()).unwrap_or_default();
        let id = ClientStr(&request_id);
        debug!("Forwarding port {port} for request {id:?}");
        // The server does not read the error stream, what a client sends on
        // it gets dropped instead of holding up the connection.
        let (_, mut error) = error.split();

        let (mut from_client, to_client) = data.split();
        let result = match Self::establish(&namespace, port, &mut from_client).await {
            Ok((connection, early)) => Self::copy(early, from_client, to_client, connection).await,
            Err(e) => {
                // Dropping the sending side resets the data stream, what the
                // client sent on it gives its budget back.
                drop((from_client, to_client));
                Err(e)
            }
        };
        match result {
            Ok(()) => debug!("Done forwarding port {port} for request {id:?}"),
            Err(e) => {
                let message = Self::forward_error(port, &e);
                debug!("Port forward request {id:?}: {message}");
                if let Err(e) = error.send(message.into()).await {
                    debug!("Unable to report port forward error: {e:#}");
                }
            }
        }
        if let Err(e) = error.close().await {
            debug!("Unable to close port forward error stream: {e:#}");
        }
    }

    /// Copy between the data stream and the connection in the pod, until
    /// both sides closed or either reset. A failure resets the data stream.
    async fn copy(
        early: Early,
        mut from_client: StreamReceiver,
        mut to_client: StreamSender,
        connection: TcpStream,
    ) -> Result<()> {
        let (mut from_container, mut to_container) = connection.into_split();

        let upstream = async {
            Self::write_input(early, &mut from_client, &mut to_container).await?;
            if !from_client.is_reset() {
                to_container
                    .shutdown()
                    .await
                    .context("close the connection to the pod")?;
            }
            anyhow::Ok(())
        };

        let downstream = async {
            let mut buf = BytesMut::new();
            loop {
                let data = select! {
                    data = Self::read_pod(&mut from_container, &mut buf) => data?,
                    // Nothing takes the output anymore.
                    () = to_client.reset_by_peer() => return Ok(()),
                };
                let Some(data) = data else {
                    break;
                };
                if let Err(e) = to_client.send(data).await {
                    return Self::unless_reset(&to_client, e);
                }
            }
            match to_client.close().await {
                Ok(()) => Ok(()),
                Err(e) => Self::unless_reset(&to_client, e),
            }
        };

        let mut upstream = pin!(upstream);
        let mut downstream = pin!(downstream);
        select! {
            result = &mut downstream => {
                result?;
                if time::timeout(CLOSE_GRACE_PERIOD, upstream).await.is_err() {
                    debug!("Client did not close its side of the connection");
                }
                Ok(())
            }
            // The client closed its side, the pod may still answer.
            result = &mut upstream => {
                result?;
                downstream.await
            }
        }
    }

    /// A stream the client reset or whose connection ended is no failure.
    fn unless_reset(sender: &StreamSender, e: anyhow::Error) -> Result<()> {
        if sender.is_reset() { Ok(()) } else { Err(e) }
    }
}
