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
fn debug_output_hides_data() {
    let frame = Frame::Data {
        stream_id: 1,
        fin: true,
        data: Bytes::from_static(b"secret"),
    };
    assert_eq!(
        format!("{frame:?}"),
        "Data { stream_id: 1, fin: true, len: 6 }"
    );
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
    control.extend_from_slice(&u32::try_from(MAX_CONTROL_FRAME_LEN + 1)?.to_be_bytes()[1..]);
    assert!(Decoder::default().read(&mut &control[..]).await.is_err());
    Ok(())
}

#[tokio::test]
#[expect(clippy::cast_possible_truncation, reason = "a wrapping byte pattern")]
async fn large_data_frames_come_in_parts() -> Result<()> {
    // The largest frame the protocol allows, with FIN, then a ping.
    let len = MAX_DATA_FRAME_LEN;
    let mut bytes = vec![0, 0, 0, 1, FLAG_FIN];
    bytes.extend_from_slice(&u32::try_from(len)?.to_be_bytes()[1..]);
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
