use super::{channels::ChannelOutput, requests::Pending, *};
use crate::{
    streaming_server::{
        spdy::{
            Connection, Headers, MAX_STREAMS,
            testing::{Client, Frame},
        },
        web::{self, Message},
    },
    test_util::{closed_port, wait_for},
};
use bytes::{BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::{Arc, atomic::Ordering},
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpSocket},
    sync::oneshot,
    task::{self, JoinSet},
};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_util::sync::CancellationToken;

mod channels;
mod connections;
mod namespace;
mod requests;
mod tunnel;

/// The network namespace of the tests, which needs no privileges to
/// connect in.
const NETNS: &str = "/proc/self/ns/net";

fn serve() -> Result<Client> {
    let (client, server) = Client::pair()?;
    task::spawn(
        PortForward::new(NETNS.into(), None)
            .without_pings()
            .serve_spdy(server),
    );
    Ok(client)
}

fn headers(typ: &str, port: u16) -> Headers {
    let port = port.to_string();
    [("streamType", typ), ("port", &port), ("requestID", "0")].into()
}

/// Open the streams of a request, the error stream closed right away
/// like clients do.
async fn request(client: &mut Client, port: u16) -> Result<()> {
    client.open(1, headers(STREAM_TYPE_ERROR, port)).await?;
    client
        .send(Frame::Data {
            stream_id: 1,
            fin: true,
            data: Bytes::new(),
        })
        .await?;
    client.open(3, headers(STREAM_TYPE_DATA, port)).await?;
    for stream_id in [1, 3] {
        assert_eq!(
            client.frame().await?,
            Some(Frame::SynReply {
                stream_id,
                headers: Headers::default()
            })
        );
    }
    Ok(())
}
