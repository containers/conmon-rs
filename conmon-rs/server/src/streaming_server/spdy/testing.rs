use super::frame::{Decoder, Encoder};
pub(crate) use super::frame::{Frame, Headers};
use anyhow::{Result, bail};
use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, duplex},
    time::{self, Duration},
};

/// The frames of what a client read from the server.
pub(crate) async fn frames(mut data: &[u8]) -> Result<Vec<Frame>> {
    let mut decoder = Decoder::default();
    let mut frames = vec![];
    while let Some(frame) = decoder.read(&mut data).await? {
        frames.push(frame);
    }
    Ok(frames)
}

/// The client side of a connection.
pub(crate) struct Client<T = DuplexStream> {
    io: T,
    encoder: Encoder,
    decoder: Decoder,
}

impl Client {
    /// A client and the server side of its connection.
    pub(crate) fn pair() -> Result<(Self, DuplexStream)> {
        let (client, server) = duplex(1024 * 1024);
        Ok((Self::new(client)?, server))
    }
}

impl<T> Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(io: T) -> Result<Self> {
        Ok(Self {
            io,
            encoder: Encoder::default(),
            decoder: Decoder::default(),
        })
    }

    pub(crate) fn io(&mut self) -> &mut T {
        &mut self.io
    }

    pub(crate) async fn send(&mut self, frame: Frame) -> Result<()> {
        let mut buf = BytesMut::new();
        self.encoder.encode(&frame, &mut buf)?;
        self.io.write_all(&buf).await?;
        Ok(())
    }

    pub(crate) async fn open(&mut self, stream_id: u32, headers: Headers) -> Result<()> {
        self.send(Frame::SynStream {
            stream_id,
            fin: false,
            headers,
        })
        .await
    }

    /// The next frame of the server. Long enough for the timeouts of the
    /// tests, which run with paused time.
    pub(crate) async fn frame(&mut self) -> Result<Option<Frame>> {
        time::timeout(Duration::from_secs(60), self.decoder.read(&mut self.io)).await?
    }

    /// Read frames until one matches, return it.
    pub(crate) async fn frame_until(&mut self, matches: impl Fn(&Frame) -> bool) -> Result<Frame> {
        loop {
            match self.frame().await? {
                Some(frame) if matches(&frame) => return Ok(frame),
                Some(_) => {}
                None => bail!("connection ended"),
            }
        }
    }
}
