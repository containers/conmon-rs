use super::frame::{Decoder, Encoder};
pub use super::frame::{Frame, Headers};
use anyhow::{Result, bail};
use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, duplex},
    time::{self, Duration},
};

/// The frames of what a client read from the server.
pub async fn frames(mut data: &[u8]) -> Result<Vec<Frame>> {
    let mut decoder = Decoder::default();
    let mut frames = vec![];
    while let Some(frame) = decoder.read(&mut data).await? {
        frames.push(frame);
    }
    Ok(frames)
}

/// The client side of a connection.
pub struct Client<T = DuplexStream> {
    io: T,
    encoder: Encoder,
    decoder: Decoder,
}

impl Client {
    /// A client and the server side of its connection.
    pub fn pair() -> Result<(Self, DuplexStream)> {
        let (client, server) = duplex(1024 * 1024);
        Ok((Self::new(client)?, server))
    }
}

impl<T> Client<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(io: T) -> Result<Self> {
        Ok(Self {
            io,
            encoder: Encoder::default(),
            decoder: Decoder::default(),
        })
    }

    pub fn io(&mut self) -> &mut T {
        &mut self.io
    }

    pub async fn send(&mut self, frame: Frame) -> Result<()> {
        let mut buf = BytesMut::new();
        self.encoder.encode(&frame, &mut buf)?;
        self.io.write_all(&buf).await?;
        Ok(())
    }

    pub async fn open(&mut self, stream_id: u32, headers: Headers) -> Result<()> {
        self.send(Frame::SynStream {
            stream_id,
            fin: false,
            headers,
        })
        .await
    }

    /// The next frame of the server. Long enough for the timeouts of the
    /// tests, which run with paused time.
    pub async fn frame(&mut self) -> Result<Option<Frame>> {
        time::timeout(Duration::from_secs(60), self.decoder.read(&mut self.io)).await?
    }

    /// Read frames until one matches, return it.
    pub async fn frame_until(&mut self, matches: impl Fn(&Frame) -> bool) -> Result<Frame> {
        loop {
            match self.frame().await? {
                Some(frame) if matches(&frame) => return Ok(frame),
                Some(_) => {}
                None => bail!("connection ended"),
            }
        }
    }
}
