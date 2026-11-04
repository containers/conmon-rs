use super::*;
use crate::{
    container_io::{ContainerIO, Message as IOMessage, Pipe, SharedContainerIO},
    container_log::ContainerLog,
    streaming_server::{
        AttachSession, MAX_MESSAGE_SIZE, StreamingServer,
        idle::STALL_TIMEOUT,
        remotecommand::{
            ClientEvent, MESSAGE_TOO_LARGE, STDIN_BYTE, STDOUT_BYTE, STREAM_ERR_BYTE, Status, Stdin,
        },
        session::PENDING_STDIN_MAX_BYTES,
        spdy::testing::{Client, Frame},
        testing::*,
        web::CloseCode,
    },
    test_util::wait_for,
};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;

use nix::sys::socket::{setsockopt, sockopt};
use std::{collections::VecDeque, ops::ControlFlow};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::watch,
    task::LocalSet,
    time::{self, Duration},
};

mod idle;
mod limits;
mod pending_stdin;
mod resize;
mod spdy_streams;
mod stdin;

/// The output of a SPDY session with stdout over a small connection in
/// memory, and its client, which reads only when told to.
async fn spdy_output(idle_timeout: Option<Duration>) -> Result<(Output, Client<DuplexStream>)> {
    let (client, server) = tokio::io::duplex(1024);
    let mut connection =
        Connection::serve(server, None, None, |h| SpdyStreams::accept(h, Version::V4));
    let mut client = Client::new(client)?;
    client.open(1, [("streamType", "error")].into()).await?;
    client.open(3, [("streamType", "stdout")].into()).await?;
    let expected = ExpectedStreams {
        stdin: false,
        stdout: true,
        stderr: false,
    };
    let streams = SpdyStreams::wait(&mut connection, expected).await?;
    let (_input, output) = streams.into_parts(connection, Version::V4).await;
    Ok((Output::spdy(output, idle_timeout), client))
}
