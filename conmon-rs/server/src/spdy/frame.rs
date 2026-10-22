//! The SPDY/3.1 frames and their encoding, see
//! <https://www.chromium.org/spdy/spdy-protocol/spdy-protocol-draft3-1/>.
//!
//! Header blocks are compressed with zlib and a preset dictionary. Each
//! direction of a connection keeps one compression context for all its header
//! blocks, so every frame which carries one has to pass the same [`Decoder`]
//! or [`Encoder`], also the ones which are otherwise ignored. The server only
//! sends empty header blocks, the [`Encoder`] stores them uncompressed in the
//! zlib stream instead of compressing them.

use anyhow::{Context, Result, bail, ensure};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use flate2::{Decompress, FlushDecompress, Status};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The protocol version of the control frames.
const VERSION: u16 = 3;

/// The length of the common header of all frames.
const HEADER_LEN: usize = 8;

const TYPE_SYN_STREAM: u16 = 1;
const TYPE_SYN_REPLY: u16 = 2;
const TYPE_RST_STREAM: u16 = 3;
const TYPE_PING: u16 = 6;
const TYPE_GOAWAY: u16 = 7;
const TYPE_HEADERS: u16 = 8;

/// The flag which ends the sending side of a stream, on SYN_STREAM, SYN_REPLY,
/// HEADERS and data frames.
const FLAG_FIN: u8 = 0x01;

/// The largest control frame a client may send. The ones Kubernetes clients
/// send carry a few short headers.
const MAX_CONTROL_FRAME_LEN: usize = 64 * 1024;

/// The largest frame the protocol allows, its length has 24 bits.
pub const MAX_DATA_FRAME_LEN: usize = 0xff_ffff;

/// How much of a data frame the server reads at once. Larger frames get
/// handed to their stream in parts, so that the server does not buffer them
/// as a whole. Go clients send 32 KiB per frame.
const DATA_CHUNK_LEN: usize = 64 * 1024;

/// The largest decompressed header block, which bounds what a small
/// compressed block may expand to. Kubernetes clients send around a hundred
/// bytes, a stream holds its headers as long as it is open. A larger block
/// fails the connection, like any header block which does not decode, the
/// compression context of the connection would be out of sync otherwise.
const MAX_HEADER_BLOCK_LEN: usize = 8 * 1024;

/// The most headers of a stream, Kubernetes clients send up to three. Each
/// takes more memory than the bytes of its block.
const MAX_HEADERS: usize = 32;

/// RST_STREAM status: the frame violates the protocol.
pub const STATUS_PROTOCOL_ERROR: u32 = 1;

/// RST_STREAM status: the stream got refused before any processing.
pub const STATUS_REFUSED_STREAM: u32 = 3;

/// RST_STREAM status: the stream is not needed anymore. spdystream resets
/// with it as well, also for streams it rejects.
pub const STATUS_CANCEL: u32 = 5;

/// GOAWAY status: the connection ends normally.
pub const GOAWAY_OK: u32 = 0;

/// GOAWAY status: the client violated the protocol.
pub const GOAWAY_PROTOCOL_ERROR: u32 = 1;

/// A single SPDY frame. Only the frames and fields the server acts on are
/// represented, see [`Frame::Ignored`].
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// The client opens a stream.
    SynStream {
        stream_id: u32,
        fin: bool,
        headers: Headers,
    },

    /// The server accepts a stream.
    SynReply { stream_id: u32, headers: Headers },

    /// Either side aborts a stream.
    RstStream { stream_id: u32, status: u32 },

    /// A ping, which the receiver echoes if the other side started it.
    Ping { id: u32 },

    /// Either side ends the connection.
    GoAway { last_stream_id: u32, status: u32 },

    /// Additional headers, which only matter for their FIN flag.
    Headers { stream_id: u32, fin: bool },

    /// Data of a stream, an empty one with FIN closes its sending side.
    Data {
        stream_id: u32,
        fin: bool,
        data: Bytes,
    },

    /// A frame which needs no handling: SETTINGS, WINDOW_UPDATE or an unknown
    /// control frame. spdystream does not do flow control, so window updates
    /// are not needed either.
    Ignored,
}

/// The headers of a stream. SPDY requires lowercase names, multiple values
/// of a name are joined with NUL bytes.
#[derive(Debug, Default, PartialEq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// The first value of a header, the name is matched case insensitively.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.split('\0').next())
    }
}

impl<const N: usize> From<[(&str, &str); N]> for Headers {
    fn from(headers: [(&str, &str); N]) -> Self {
        Self(
            headers
                .into_iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
                .collect(),
        )
    }
}

/// Reads the frames of one direction of a connection.
pub struct Decoder {
    decompress: Decompress,

    /// The data frame which is handed out in parts: its stream, the bytes
    /// still to read and its FIN flag.
    partial_data: Option<(u32, usize, bool)>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            decompress: Decompress::new(true),
            partial_data: None,
        }
    }
}

impl Decoder {
    /// Read the next frame, `None` if the connection ended in between frames.
    /// A data frame larger than [`DATA_CHUNK_LEN`] comes as several ones of
    /// the same stream, only the last one with its FIN flag.
    pub async fn read<R>(&mut self, reader: &mut R) -> Result<Option<Frame>>
    where
        R: AsyncRead + Unpin,
    {
        if let Some((stream_id, remaining, fin)) = self.partial_data {
            return self
                .read_data(reader, stream_id, remaining, fin)
                .await
                .map(Some);
        }

        let mut header = [0; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match reader
                .read(&mut header[filled..])
                .await
                .context("read frame header")?
            {
                0 if filled == 0 => return Ok(None),
                0 => bail!("connection ended within a frame header"),
                n => filled += n,
            }
        }

        let control = header[0] & 0x80 != 0;
        let flags = header[4];
        let len =
            usize::from(header[5]) << 16 | usize::from(header[6]) << 8 | usize::from(header[7]);
        if !control && len > DATA_CHUNK_LEN {
            let stream_id = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            return self
                .read_data(reader, stream_id, len, flags & FLAG_FIN != 0)
                .await
                .map(Some);
        }
        ensure!(
            len <= MAX_CONTROL_FRAME_LEN,
            "frame of {len} bytes exceeds {MAX_CONTROL_FRAME_LEN} bytes"
        );

        let mut payload = BytesMut::zeroed(len);
        reader
            .read_exact(&mut payload)
            .await
            .context("read frame payload")?;
        self.decode(header, flags, payload.freeze()).map(Some)
    }

    /// Read the next part of a large data frame.
    async fn read_data<R>(
        &mut self,
        reader: &mut R,
        stream_id: u32,
        remaining: usize,
        fin: bool,
    ) -> Result<Frame>
    where
        R: AsyncRead + Unpin,
    {
        let len = remaining.min(DATA_CHUNK_LEN);
        let mut data = BytesMut::zeroed(len);
        reader
            .read_exact(&mut data)
            .await
            .context("read frame payload")?;
        let remaining = remaining - len;
        self.partial_data = (remaining > 0).then_some((stream_id, remaining, fin));
        Ok(Frame::Data {
            stream_id,
            fin: fin && remaining == 0,
            data: data.freeze(),
        })
    }

    /// Decode a frame from its header and complete payload.
    fn decode(&mut self, header: [u8; HEADER_LEN], flags: u8, mut payload: Bytes) -> Result<Frame> {
        let fin = flags & FLAG_FIN != 0;
        let first = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);

        if first & 0x8000_0000 == 0 {
            return Ok(Frame::Data {
                stream_id: first,
                fin,
                data: payload,
            });
        }

        let version = (first >> 16) as u16 & 0x7fff;
        ensure!(version == VERSION, "unsupported SPDY version {version}");

        let frame = match first as u16 {
            TYPE_SYN_STREAM => {
                // The stream ID, the associated stream ID, the priority and
                // the slot precede the header block.
                ensure!(payload.len() >= 10, "SYN_STREAM too short");
                let stream_id = payload.get_u32() & 0x7fff_ffff;
                payload.advance(6);
                let headers = self.header_block(&payload)?;
                Frame::SynStream {
                    stream_id,
                    fin,
                    headers,
                }
            }
            TYPE_SYN_REPLY => {
                ensure!(payload.len() >= 4, "SYN_REPLY too short");
                let stream_id = payload.get_u32() & 0x7fff_ffff;
                let headers = self.header_block(&payload)?;
                Frame::SynReply { stream_id, headers }
            }
            TYPE_RST_STREAM => {
                ensure!(payload.len() == 8, "RST_STREAM has the wrong length");
                Frame::RstStream {
                    stream_id: payload.get_u32() & 0x7fff_ffff,
                    status: payload.get_u32(),
                }
            }
            TYPE_PING => {
                ensure!(payload.len() == 4, "PING has the wrong length");
                Frame::Ping {
                    id: payload.get_u32(),
                }
            }
            TYPE_GOAWAY => {
                ensure!(payload.len() == 8, "GOAWAY has the wrong length");
                Frame::GoAway {
                    last_stream_id: payload.get_u32() & 0x7fff_ffff,
                    status: payload.get_u32(),
                }
            }
            TYPE_HEADERS => {
                ensure!(payload.len() >= 4, "HEADERS too short");
                let stream_id = payload.get_u32() & 0x7fff_ffff;
                self.header_block(&payload)?;
                Frame::Headers { stream_id, fin }
            }
            _ => Frame::Ignored,
        };
        Ok(frame)
    }

    /// Decompress and parse a header block.
    fn header_block(&mut self, mut input: &[u8]) -> Result<Headers> {
        // One byte more than the limit, so that a block of exactly the limit
        // still leaves room, which tells that it is complete.
        let max_capacity = MAX_HEADER_BLOCK_LEN + 1;
        let mut block = Vec::with_capacity((input.len() * 4).min(max_capacity));
        loop {
            if block.len() == block.capacity() {
                ensure!(
                    block.len() <= MAX_HEADER_BLOCK_LEN,
                    "header block exceeds {MAX_HEADER_BLOCK_LEN} bytes"
                );
                // Exactly, a larger capacity would let the block grow past
                // the limit.
                block.reserve_exact((max_capacity - block.len()).min(4096));
            }

            let (total_in, total_out) = (self.decompress.total_in(), self.decompress.total_out());
            let status =
                match self
                    .decompress
                    .decompress_vec(input, &mut block, FlushDecompress::Sync)
                {
                    Ok(status) => status,
                    Err(e) if e.needs_dictionary().is_some() => {
                        self.decompress
                            .set_dictionary(DICTIONARY)
                            .context("set header dictionary")?;
                        Status::Ok
                    }
                    Err(e) => return Err(e).context("decompress header block"),
                };
            input = &input[(self.decompress.total_in() - total_in) as usize..];
            // The allocator may still have given more room than asked for.
            ensure!(
                block.len() <= MAX_HEADER_BLOCK_LEN,
                "header block exceeds {MAX_HEADER_BLOCK_LEN} bytes"
            );

            // The sender flushes after each block, so all of it is there once
            // the input is consumed and the output has room left.
            if status == Status::StreamEnd || input.is_empty() && block.len() < block.capacity() {
                break;
            }
            ensure!(
                block.len() == block.capacity()
                    || self.decompress.total_in() > total_in
                    || self.decompress.total_out() > total_out,
                "header block does not decompress"
            );
        }

        Self::parse_header_block(&block)
    }

    fn parse_header_block(mut block: &[u8]) -> Result<Headers> {
        fn string(block: &mut &[u8]) -> Result<String> {
            ensure!(block.len() >= 4, "header block truncated");
            let len = block.get_u32() as usize;
            ensure!(block.len() >= len, "header block truncated");
            let s = String::from_utf8_lossy(&block[..len]).into_owned();
            block.advance(len);
            Ok(s)
        }

        ensure!(block.len() >= 4, "header block truncated");
        let count = block.get_u32() as usize;
        ensure!(
            count <= MAX_HEADERS,
            "header block has more than {MAX_HEADERS} headers"
        );
        // Each pair takes at least the two lengths.
        ensure!(count <= block.len() / 8, "header block truncated");
        let mut headers = Vec::with_capacity(count);
        for _ in 0..count {
            let name = string(&mut block)?;
            let value = string(&mut block)?;
            headers.push((name, value));
        }
        Ok(Headers(headers))
    }
}

/// Writes the frames of one direction of a connection.
#[derive(Default)]
pub struct Encoder {
    /// Whether the zlib header went out with the first header block.
    started: bool,
}

impl Encoder {
    /// Append the encoded frame to the buffer.
    pub fn encode(&mut self, frame: &Frame, buf: &mut BytesMut) -> Result<()> {
        match frame {
            Frame::SynStream {
                stream_id,
                fin,
                headers,
            } => {
                let block = self.header_block(headers);
                Self::control(buf, TYPE_SYN_STREAM, *fin, 10 + block.len());
                buf.put_u32(*stream_id);
                // No associated stream, the lowest priority and no slot.
                buf.put_u32(0);
                buf.put_u16(0);
                buf.put_slice(&block);
            }
            Frame::SynReply { stream_id, headers } => {
                let block = self.header_block(headers);
                Self::control(buf, TYPE_SYN_REPLY, false, 4 + block.len());
                buf.put_u32(*stream_id);
                buf.put_slice(&block);
            }
            Frame::RstStream { stream_id, status } => {
                Self::control(buf, TYPE_RST_STREAM, false, 8);
                buf.put_u32(*stream_id);
                buf.put_u32(*status);
            }
            Frame::Ping { id } => {
                Self::control(buf, TYPE_PING, false, 4);
                buf.put_u32(*id);
            }
            Frame::GoAway {
                last_stream_id,
                status,
            } => {
                Self::control(buf, TYPE_GOAWAY, false, 8);
                buf.put_u32(*last_stream_id);
                buf.put_u32(*status);
            }
            Frame::Headers { stream_id, fin } => {
                let block = self.header_block(&Headers::default());
                Self::control(buf, TYPE_HEADERS, *fin, 4 + block.len());
                buf.put_u32(*stream_id);
                buf.put_slice(&block);
            }
            Frame::Data {
                stream_id,
                fin,
                data,
            } => {
                ensure!(data.len() <= MAX_DATA_FRAME_LEN, "data frame too large");
                buf.put_u32(*stream_id);
                buf.put_u8(if *fin { FLAG_FIN } else { 0 });
                Self::put_len(buf, data.len());
                buf.put_slice(data);
            }
            Frame::Ignored => {}
        }
        Ok(())
    }

    fn control(buf: &mut BytesMut, typ: u16, fin: bool, len: usize) {
        buf.put_u16(0x8000 | VERSION);
        buf.put_u16(typ);
        buf.put_u8(if fin { FLAG_FIN } else { 0 });
        Self::put_len(buf, len);
    }

    fn put_len(buf: &mut BytesMut, len: usize) {
        buf.put_slice(&(len as u32).to_be_bytes()[1..]);
    }

    /// Serialize a header block into stored deflate blocks, which every
    /// zlib decompressor reads like compressed ones. Compressing the blocks
    /// of the server would gain nothing, they are empty.
    fn header_block(&mut self, headers: &Headers) -> Vec<u8> {
        self.deflate(&Self::serialize(headers))
    }

    /// The uncompressed header block.
    fn serialize(headers: &Headers) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.put_u32(headers.0.len() as u32);
        for (name, value) in &headers.0 {
            raw.put_u32(name.len() as u32);
            raw.put_slice(name.as_bytes());
            raw.put_u32(value.len() as u32);
            raw.put_slice(value.as_bytes());
        }
        raw
    }

    /// Wrap a serialized header block into stored deflate blocks, the first
    /// one of the connection with the zlib header.
    fn deflate(&mut self, raw: &[u8]) -> Vec<u8> {
        let mut block = Vec::with_capacity(raw.len() + 16);
        if !self.started {
            // Deflate with a 32 KiB window and the preset dictionary.
            block.put_slice(&[0x78, 0x20]);
            block.put_u32(DICTIONARY_ID);
            self.started = true;
        }
        for chunk in raw.chunks(usize::from(u16::MAX)) {
            Self::stored_block(&mut block, chunk);
        }
        // The empty block of a sync flush, Go hands out the data before it
        // only once it read one. Without it, the decompressor reads beyond
        // the frame and fails all later header blocks.
        Self::stored_block(&mut block, &[]);
        block
    }

    /// A stored deflate block which is not the last one.
    fn stored_block(block: &mut Vec<u8>, data: &[u8]) {
        let len = data.len() as u16;
        block.put_u8(0);
        block.put_u16_le(len);
        block.put_u16_le(!len);
        block.put_slice(data);
    }
}

/// The entry points of the fuzz targets.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::{Decoder, Encoder, MAX_HEADER_BLOCK_LEN};
    use futures::FutureExt;

    /// Decode frames until the input ends or is invalid. Reading a slice is
    /// never pending, so no runtime is needed.
    pub fn frames(mut input: &[u8]) {
        let mut decoder = Decoder::default();
        while let Some(Ok(Some(_))) = decoder.read(&mut input).now_or_never() {}
    }

    /// Decode the input as a compressed header block, which is what a
    /// client controls, parse it as a decompressed one, and decode it once
    /// deflated like the server does, which always gets past the
    /// decompression to the parsing. Headers which got parsed come back the
    /// same once encoded, unless they exceed the limit.
    pub fn header_block(raw: &[u8]) {
        let _ = Decoder::default().header_block(raw);
        let block = Encoder::default().deflate(raw);
        let decoded = Decoder::default().header_block(&block);
        let Ok(headers) = Decoder::parse_header_block(raw) else {
            assert!(decoded.is_err());
            return;
        };
        let serialized = Encoder::serialize(&headers);
        let decoded = Decoder::default().header_block(&Encoder::default().deflate(&serialized));
        if serialized.len() <= MAX_HEADER_BLOCK_LEN {
            assert_eq!(decoded.ok(), Some(headers));
        } else {
            assert!(decoded.is_err());
        }
    }
}

/// The Adler-32 checksum of the dictionary, which the zlib header names.
const DICTIONARY_ID: u32 = 0xe3c6a7c2;

/// The preset dictionary for the header compression, from the specification.
const DICTIONARY: &[u8; 1423] = b"\
    \0\0\0\x07options\0\0\0\x04head\0\0\0\x04post\0\0\0\x03put\0\0\0\x06delete\
    \0\0\0\x05trace\0\0\0\x06accept\0\0\0\x0eaccept-charset\
    \0\0\0\x0faccept-encoding\0\0\0\x0faccept-language\0\0\0\x0daccept-ranges\
    \0\0\0\x03age\0\0\0\x05allow\0\0\0\x0dauthorization\0\0\0\x0dcache-control\
    \0\0\0\x0aconnection\0\0\0\x0ccontent-base\0\0\0\x10content-encoding\
    \0\0\0\x10content-language\0\0\0\x0econtent-length\0\0\0\x10content-location\
    \0\0\0\x0bcontent-md5\0\0\0\x0dcontent-range\0\0\0\x0ccontent-type\
    \0\0\0\x04date\0\0\0\x04etag\0\0\0\x06expect\0\0\0\x07expires\0\0\0\x04from\
    \0\0\0\x04host\0\0\0\x08if-match\0\0\0\x11if-modified-since\
    \0\0\0\x0dif-none-match\0\0\0\x08if-range\0\0\0\x13if-unmodified-since\
    \0\0\0\x0dlast-modified\0\0\0\x08location\0\0\0\x0cmax-forwards\
    \0\0\0\x06pragma\0\0\0\x12proxy-authenticate\0\0\0\x13proxy-authorization\
    \0\0\0\x05range\0\0\0\x07referer\0\0\0\x0bretry-after\0\0\0\x06server\
    \0\0\0\x02te\0\0\0\x07trailer\0\0\0\x11transfer-encoding\0\0\0\x07upgrade\
    \0\0\0\x0auser-agent\0\0\0\x04vary\0\0\0\x03via\0\0\0\x07warning\
    \0\0\0\x10www-authenticate\0\0\0\x06method\0\0\0\x03get\0\0\0\x06status\
    \0\0\0\x06200 OK\0\0\0\x07version\0\0\0\x08HTTP/1.1\0\0\0\x03url\
    \0\0\0\x06public\0\0\0\x0aset-cookie\0\0\0\x0akeep-alive\0\0\0\x06origin100\
    101201202205206300302303304305306307402405406407408409410411412413414415416\
    417502504505203 Non-Authoritative Information204 No Content301 Moved \
    Permanently400 Bad Request401 Unauthorized403 Forbidden404 Not Found500 \
    Internal Server Error501 Not Implemented503 Service UnavailableJan Feb Mar \
    Apr May Jun Jul Aug Sept Oct Nov Dec 00:00:00 Mon, Tue, Wed, Thu, Fri, Sat, \
    Sun, GMTchunked,text/html,image/png,image/jpg,image/gif,application/xml,\
    application/xhtml+xml,text/plain,text/javascript,publicprivatemax-age=gzip,\
    deflate,sdchcharset=utf-8charset=iso-8859-1,utf-,*,enq=0.";

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    /// Frames moby/spdystream v0.5.1 wrote, as a Kubernetes port forward
    /// client sends them for its first request.
    const GO_SYN_STREAM_ERROR: &str = "80030001000000430000000100000000000078f9e3c6a7c2626060608694bf452520dac2c0c280818181b308125f99290c0c0c8c06e0945852949a980b2d6c58538b8af28b00000000ffff";
    const GO_SYN_STREAM_DATA: &str =
        "800300010000001b00000003000000000000826a46956249492c49246c24000000ffff";
    const GO_DATA_FIN: &str = "0000000101000000";
    const GO_PING: &str = "800300060000000400000001";
    const GO_RST_STREAM: &str = "80030003000000080000000300000005";
    const GO_GOAWAY: &str = "80030007000000080000000300000000";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    async fn decode_all(decoder: &mut Decoder, bytes: &[u8]) -> Result<Vec<Frame>> {
        let mut reader = bytes;
        let mut frames = vec![];
        while let Some(frame) = decoder.read(&mut reader).await? {
            frames.push(frame);
        }
        Ok(frames)
    }

    #[test]
    fn dictionary_checksum() -> Result<()> {
        // The ID spdystream puts into the zlib header.
        let mut compress = Compress::new(Compression::default(), true);
        assert_eq!(compress.set_dictionary(DICTIONARY)?, DICTIONARY_ID);
        Ok(())
    }

    #[test]
    fn zlib_header_is_valid() {
        // The check bits make the first two bytes a multiple of 31, the
        // dictionary flag is set.
        let header = u16::from_be_bytes([0x78, 0x20]);
        assert_eq!(header % 31, 0);
        assert_ne!(header & 0x20, 0);
    }

    #[tokio::test]
    async fn decode_go_frames() -> Result<()> {
        let bytes = [
            GO_SYN_STREAM_ERROR,
            GO_SYN_STREAM_DATA,
            GO_DATA_FIN,
            GO_PING,
            GO_RST_STREAM,
            GO_GOAWAY,
        ]
        .map(hex)
        .concat();

        let frames = decode_all(&mut Decoder::default(), &bytes).await?;
        assert_eq!(frames.len(), 6);
        // Go writes the headers in random order.
        for (frame, (id, typ)) in frames.iter().zip([(1, "error"), (3, "data")]) {
            let Frame::SynStream {
                stream_id,
                fin,
                headers,
            } = frame
            else {
                panic!("not a SYN_STREAM: {frame:?}");
            };
            assert_eq!((*stream_id, *fin), (id, false));
            assert_eq!(headers.get("streamType"), Some(typ));
            assert_eq!(headers.get("port"), Some("8080"));
            assert_eq!(headers.get("requestID"), Some("0"));
            assert_eq!(headers.get("missing"), None);
        }
        assert_eq!(
            frames[2..],
            [
                Frame::Data {
                    stream_id: 1,
                    fin: true,
                    data: Bytes::new(),
                },
                Frame::Ping { id: 1 },
                Frame::RstStream {
                    stream_id: 3,
                    status: STATUS_CANCEL,
                },
                Frame::GoAway {
                    last_stream_id: 3,
                    status: GOAWAY_OK,
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn encode_go_frames() -> Result<()> {
        // The frames without a header block are the same bytes Go writes.
        let mut encoder = Encoder::default();
        for (frame, expected) in [
            (
                Frame::Data {
                    stream_id: 1,
                    fin: true,
                    data: Bytes::new(),
                },
                GO_DATA_FIN,
            ),
            (Frame::Ping { id: 1 }, GO_PING),
            (
                Frame::RstStream {
                    stream_id: 3,
                    status: STATUS_CANCEL,
                },
                GO_RST_STREAM,
            ),
            (
                Frame::GoAway {
                    last_stream_id: 3,
                    status: GOAWAY_OK,
                },
                GO_GOAWAY,
            ),
        ] {
            let mut buf = BytesMut::new();
            encoder.encode(&frame, &mut buf)?;
            assert_eq!(buf, hex(expected));
        }
        Ok(())
    }

    #[test]
    fn encode_header_blocks_go_reads() -> Result<()> {
        // Two SYN_REPLY frames moby/spdystream v0.5.1 decodes, the second
        // one continues the zlib stream of the first.
        let mut encoder = Encoder::default();
        let mut buf = BytesMut::new();
        for stream_id in [1, 3] {
            encoder.encode(
                &Frame::SynReply {
                    stream_id,
                    headers: Headers::default(),
                },
                &mut buf,
            )?;
        }
        assert_eq!(
            buf,
            hex(concat!(
                "800300020000001800000001",
                "7820e3c6a7c2",
                "000400fbff00000000",
                "000000ffff",
                "800300020000001200000003",
                "000400fbff00000000",
                "000000ffff",
            ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn header_blocks_round_trip() -> Result<()> {
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::default();
        let frames = [
            Frame::SynStream {
                stream_id: 1,
                fin: true,
                headers: [("streamtype", "error"), ("port", "80")].into(),
            },
            Frame::SynReply {
                stream_id: 1,
                headers: Headers::default(),
            },
            Frame::Headers {
                stream_id: 1,
                fin: true,
            },
            Frame::SynStream {
                stream_id: 3,
                fin: false,
                headers: [("streamtype", "data"), ("port", "80")].into(),
            },
            Frame::Data {
                stream_id: 3,
                fin: false,
                data: Bytes::from_static(b"hello"),
            },
        ];
        let mut buf = BytesMut::new();
        for frame in &frames {
            encoder.encode(frame, &mut buf)?;
        }

        assert_eq!(decode_all(&mut decoder, &buf).await?, frames);
        Ok(())
    }

    #[test]
    fn header_counts_are_limited() -> Result<()> {
        let headers = |n| Headers((0..n).map(|i| (format!("h{i}"), String::new())).collect());
        let raw = Encoder::serialize(&headers(MAX_HEADERS));
        assert_eq!(Decoder::parse_header_block(&raw)?, headers(MAX_HEADERS));
        let raw = Encoder::serialize(&headers(MAX_HEADERS + 1));
        assert!(Decoder::parse_header_block(&raw).is_err());
        Ok(())
    }

    #[test]
    fn multiple_values_are_split() {
        let headers = Headers::from([("port", "80\081")]);
        assert_eq!(headers.get("port"), Some("80"));
    }

    #[tokio::test]
    async fn oversized_control_frames_are_rejected() -> Result<()> {
        let mut control = vec![0x80, 0x03, 0, 1, 0];
        control.extend_from_slice(&((MAX_CONTROL_FRAME_LEN + 1) as u32).to_be_bytes()[1..]);
        assert!(Decoder::default().read(&mut &control[..]).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn large_data_frames_come_in_parts() -> Result<()> {
        // The largest frame the protocol allows, with FIN, then a ping.
        let len = MAX_DATA_FRAME_LEN;
        let mut bytes = vec![0, 0, 0, 1, FLAG_FIN];
        bytes.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
        bytes.extend((0..len).map(|i| i as u8));
        bytes.extend(hex(GO_PING));

        let frames = decode_all(&mut Decoder::default(), &bytes).await?;
        assert_eq!(frames.len(), len.div_ceil(DATA_CHUNK_LEN) + 1);
        let mut received = vec![];
        for (i, frame) in frames[..frames.len() - 1].iter().enumerate() {
            let Frame::Data {
                stream_id: 1,
                fin,
                data,
            } = frame
            else {
                panic!("not a data frame of stream 1: {frame:?}");
            };
            assert!(data.len() <= DATA_CHUNK_LEN);
            // Only the last part ends the stream.
            assert_eq!(*fin, i == frames.len() - 2);
            received.extend_from_slice(data);
        }
        assert!(received.iter().enumerate().all(|(i, b)| *b == i as u8));
        assert_eq!(received.len(), len);
        assert_eq!(frames.last(), Some(&Frame::Ping { id: 1 }));

        // A frame which ends within its payload is an error.
        let mut decoder = Decoder::default();
        let truncated = &bytes[..HEADER_LEN + DATA_CHUNK_LEN + 1];
        let mut reader = truncated;
        decoder.read(&mut reader).await?;
        assert!(decoder.read(&mut reader).await.is_err());
        Ok(())
    }

    /// A HEADERS frame whose block decompresses to `len` zeros, which is a
    /// block without headers followed by padding.
    fn headers_frame(len: usize) -> Result<BytesMut> {
        let mut compress = Compress::new(Compression::best(), true);
        compress.set_dictionary(DICTIONARY)?;
        let mut block = Vec::with_capacity(16 * 1024);
        compress.compress_vec(&vec![0; len], &mut block, FlushCompress::Sync)?;
        assert!(block.len() < MAX_CONTROL_FRAME_LEN);

        let mut frame = BytesMut::new();
        Encoder::control(&mut frame, TYPE_HEADERS, false, 4 + block.len());
        frame.put_u32(1);
        frame.put_slice(&block);
        Ok(frame)
    }

    #[tokio::test]
    async fn header_bombs_are_rejected() -> Result<()> {
        // A few hundred compressed bytes which expand to a megabyte.
        let frame = headers_frame(1024 * 1024)?;
        let err = Decoder::default()
            .read(&mut &frame[..])
            .await
            .expect_err("bomb");
        assert!(err.to_string().contains("exceeds"), "{err:#}");
        Ok(())
    }

    #[tokio::test]
    async fn header_blocks_are_limited_exactly() -> Result<()> {
        for len in [MAX_HEADER_BLOCK_LEN - 1, MAX_HEADER_BLOCK_LEN] {
            let frame = headers_frame(len)?;
            assert_eq!(
                Decoder::default().read(&mut &frame[..]).await?,
                Some(Frame::Headers {
                    stream_id: 1,
                    fin: false
                }),
                "{len}"
            );
        }
        // Past the limit, but within what growing the block by doubling its
        // capacity would allow.
        for len in [MAX_HEADER_BLOCK_LEN + 1, 70 * 1024, 100 * 1024] {
            let frame = headers_frame(len)?;
            let err = Decoder::default()
                .read(&mut &frame[..])
                .await
                .expect_err("too large");
            assert!(err.to_string().contains("exceeds"), "{len}: {err:#}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn truncated_frames_are_rejected() -> Result<()> {
        let bytes = hex(GO_PING);
        assert!(Decoder::default().read(&mut &bytes[..5]).await.is_err());
        assert!(Decoder::default().read(&mut &bytes[..10]).await.is_err());
        assert!(Decoder::default().read(&mut &[][..]).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn wrong_versions_are_rejected() -> Result<()> {
        let mut bytes = hex(GO_PING);
        bytes[1] = 2;
        assert!(Decoder::default().read(&mut &bytes[..]).await.is_err());
        Ok(())
    }
}
