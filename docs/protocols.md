# Protocols and features

## Cap'n Proto RPC

All operations are RPCs on the `conmon.sock` UNIX domain socket, defined in
[conmon-rs/server/proto/conmon.capnp](../conmon-rs/server/proto/conmon.capnp):
`version`, `createContainer`, `execSyncContainer`, `attachContainer`,
`reopenLogContainer`, `setWindowSizeContainer`, `createNamespaces`,
`startFdSocket`, `serveExecContainer`, `serveAttachContainer` and
`servePortForwardContainer`. The Go client wraps each of them in a method with
plain Go request and response structures. An OCI runtime which hangs gets
killed and its call fails: for `createContainer` after 10 minutes, for an exec
60 seconds after its timeout, or after 60 seconds without one, as the runtime
only starts the exec process. Each runtime runs in a process group of its own,
which gets killed with it, so the helpers it started do not stay behind.

Since unreleased, `execSyncContainer` returns at most 16 MiB less 64 KiB of
output, stdout and stderr together, like conmon, so that the response fits
into the 16 MiB gRPC messages of CRI-O and the kubelet. Each stream keeps up
to half of it and more of what the other one leaves, so a large stdout does
not drop a short error on stderr. Output beyond the limit gets dropped without
notice. Before, stdout and stderr took up to 16 MiB each.

A call which fails carries a message of the server. Only the prefixes which
the comment of the `Conmon` interface in the schema lists stay stable, like
`container not found` for a container the server does not know, the Go client
matches them with errors like `ErrContainerNotFound`.

## Attach socket

`attachContainer` makes the server listen on a `SOCK_SEQPACKET` UNIX socket at
the requested path, which has to be absolute since v1.2.2. Packets from the
client are the container's stdin, passed on as they are, NUL bytes included. A
packet of more than 33 KiB (64 KiB before v1.2.2) gets truncated to its
first 33 KiB. The Go client sends one read of its stdin per packet, at most
32 KiB plus the start of a detach key sequence which it held back. The client's
input ends when it shuts down the write side of its connection. Packets to the
client start with one byte selecting the stream, `2` for stdout and `3` for
stderr, followed by up to 8192 bytes of data, since v1.2.2. Before, a
packet had at most 8191 bytes of data, which split every full read of 8 KiB in
two. A single `0` byte tells the client that the container output ended. This
is the same protocol conmon speaks, so existing attach clients work unchanged.

With `stopAfterStdinEof`, the server closes the container's stdin once the
input of a client ended, the output is still forwarded until the container
closes it. The stdin of a terminal is never closed. The value of the latest
attach call of a container applies to the connections the server accepts
afterwards on any of its attach sockets, so all attach calls of a container
should agree on it, CRI-O derives it from the container configuration.

The container output never waits for an attach client. A client which falls
more than 128 reads of the output behind, of up to 8 KiB each, about 1 MiB,
skips the oldest output, which the server logs as a warning, once until the
client caught up again. The same applies to the attach sessions of the
streaming server.

## Streaming server

`serveExecContainer`, `serveAttachContainer` and `servePortForwardContainer`
return a URL on a server the conmonrs process starts on demand on `127.0.0.1`
with a random port. Each URL carries a one time token which is valid for a
minute. At most 1000 URLs of each kind (exec and attach, port forward) wait for
their client at once, a request beyond that fails with an error starting with
`maximum number of in-flight requests exceeded` (`ErrTooManyRequests` of the Go
client), like the kubelet fails requests beyond its limit. v1.1.1 and older drop
the oldest URL instead. CRI-O passes these URLs to the kubelet, which connects
directly. Most streaming options of CRI-O do not apply to this server, see
[Configure CRI-O](cri-o.md#configure-cri-o).

### Exec and attach

Exec and attach speak the Kubernetes remote command protocols on the same
URL.

Over WebSocket, `v5.channel.k8s.io` is what kubectl uses: the first byte of
each binary message is the stream (`0` stdin, `1` stdout, `2` stderr, `3`
errors, `4` terminal resize events as JSON, `255` close). The versions before
it (`v4.channel.k8s.io`, `channel.k8s.io` and their `base64` variants) frame
the messages the same way without the close signal, base64 ones as text
messages with the stream as an ASCII digit offset, and start with an empty
message on the first output stream. Like for the Kubernetes runtime streaming
server, a client without a subprotocol gets binary `channel.k8s.io`, and one
which only requests unsupported subprotocols gets a 403.

Over SPDY/3.1, which the API server translates the WebSocket of kubectl to, the
client opens a stream per channel by the `streamType` header (`error`, `stdin`,
`stdout`, `stderr` and with a terminal `resize`, which may also come after the
start), the end of the stdin stream closes stdin. The end of the stdin of the
client, over SPDY or over WebSocket with `v5.channel.k8s.io` (the versions
before have no close signal), closes the stdin of an exec process, and the one
of an attached container with `stopAfterStdinEof`. A terminal is never closed,
it gets its end of file character instead, like from Ctrl-D. Without a terminal
and without `stopAfterStdinEof`, the end of the stdin of the client ends an
attach session. With `stopAfterStdinEof`, a client which goes away without
closing its stdin still closes the stdin of the container, unless it is a
terminal. The server keeps its side of the stdin stream open until the session
ends, except for `channel.k8s.io`, like the kubelet. Streams for stdout or
stderr which come after the start end right away. The server supports
`v4.channel.k8s.io`, `v3`, `v2` and `channel.k8s.io`, the order of the client
decides. From v4 on, the final status is JSON, before only a failure is
reported as text.

Since v1.2.2, a client which takes the end of a session slowly, the
output queued for it, the status and the close, gets all of it, only one which
takes nothing of it for 60 seconds gets its connection closed. Before, a client
which did not take all of it within a few seconds lost the rest, and client-go
took the missing status for a success. After the close, the server reads and
drops what the client still sends until it answered the WebSocket close or
closed the SPDY connection, for up to 5 seconds, as closing a connection with
unread input resets it, which may lose the end of the session. Port forwarding
does the same. Exec and attach sessions with an idle timeout wait until the
client took nothing for it if that is longer, so that a client which pauses
the output, like a pager, does not lose the end. CRI-O v1.38, `main`
and the releases of the 1.37 branch after v1.37.2 always pass one, its
`stream_idle_timeout` of 4 hours by default. A client which vanished without
a reset while its output backs up holds the end just as long, or until the
kernel gives up resending to it, after 15 to 30 minutes with the default
`tcp_retries2` of 15: there is no keepalive.

Since unreleased, a WebSocket client which closes first, also one of port
forwarding, gets the close of the server in answer as soon as the server read
it, also while the stdin before it still goes to the process. Before, the
connection ended without it, which browsers report as code 1006 and which kept
the Python Kubernetes client waiting for up to 3 seconds in `close()`.

Since v1.2.2, an exec process with a terminal gets the size the client
sends while the runtime starts it, usually before the command runs, like
with CRI-O. Before, the terminal got it only once the runtime finished, so a
short command like `kubectl exec -it -- ls` saw a terminal without rows or
columns.

An exec process which writes more than 16 MiB to stdout or stderr while the
runtime still starts it gets its output cut there. Since v1.2.2, the
session then fails with an error status instead of reporting the exit code.

Since v1.2.2, an exec process without a terminal whose client went away
gets the end of its stdin and its output pipes closed, so that its writes
fail. Before, its output got read and dropped until it exited, which still
happens with a terminal after its SIGHUP. Only the idle timeout or a lost exit
kill it.

Stdin counts towards the [idle timeout](#limits-and-timeouts) once the server
reads it. Stdin which backs up behind a process which does not read it stops
the server from reading the client, which may then go idle, like a spdystream
connection whose reader waits. Empty stdin messages do not count towards the
stdin limit of a session.

Since unreleased, the stdin which a client sent right before a clean close (the
WebSocket close, or the end of a SPDY connection after its stdin) reaches the
process as long as the process keeps taking it. Only a process which takes none
of it for 5 seconds lets the session end without the rest, counted from the
close, or for an exec from the start of its process, however long the runtime
takes to start it (the runtime gets killed after 60 seconds). Before, the
process got about a second after the close and the rest got dropped, which cut
off the uploads of clients which close right after their last write, like the
Python client or `k8s_cp` of Ansible, once the process took less than about
1 MiB per second.

### Port forwarding

Port forwarding speaks the Kubernetes `portforward.k8s.io` protocol over
SPDY/3.1, which the API server also translates the WebSocket of kubectl to.
Per forwarded connection the client opens an `error` and a `data` stream
with the `port` and `requestID` headers. The
server connects to that port on `localhost` within the network namespace of
the request, whose `netNsPath` has to be absolute since v1.2.2, and copies
between the connection and the data stream, a failure is reported as text on
the error stream, what the client sends on it gets dropped. Only what the SPDY
implementation of Kubernetes uses is supported. There is no flow control: once
a connection holds as much of what the client sent as it may, the server stops
reading it until the streams took their data. What a forwarded connection
sends while it gets established moves into a connect budget of the session
instead, so that a port which does not answer does not stop the other
connections. Once that is used up, connecting streams stop taking their data
like any stream which does not take it. Any number of connections get
established at once. Clients which connect directly, like crictl with the
WebSocket transport, may tunnel the SPDY connection through binary WebSocket
messages with the `SPDY/3.1+portforward.k8s.io` subprotocol.

The URL also serves the older WebSocket channel protocol (`v4.channel.k8s.io`,
`v4.base64.channel.k8s.io` or no subprotocol), which forwards one connection
per port of the `port` query parameter, a request with too many gets a 400.
Port `i` gets the data channel `2i` and the error channel `2i+1`, every
message starts with its channel (an ASCII digit offset in base64 text
messages) and the first message of each channel carries the port as a little
endian `u16`. The session ends once all connections ended. The request of
`servePortForwardContainer` has no ports, and the kubelet does not pass the
query of its client on, so an engine which hands out the URL for this protocol
appends the ports of the CRI `PortForwardRequest` as `port` query parameters,
like `?port=80&port=443`, as CRI-O does.

### Limits and timeouts

| What                                                           | Limit                                                                 |
| -------------------------------------------------------------- | --------------------------------------------------------------------- |
| URLs which wait for their client, per kind                     | 1000, see above                                                       |
| Time until the token of an unused URL expires                  | 60 seconds, the first connection takes it                             |
| WebSocket message or frame of the client                       | 1 MiB, 4 MiB for exec and attach since v1.2.2, more ends it           |
| Stdin of a session which the process did not take yet          | About 9 MiB, 3 MiB before v1.2.2, plus what a SPDY connection holds   |
| What the client sent on a SPDY connection which no stream took | About 4 MiB, plus the frame which waits to be placed                  |
| What a forwarded connection sends while it gets established    | 1 MiB per connection, from a connect budget of 16 MiB per session     |
| Streams of a SPDY connection                                   | 1024, which count until the server let go of them, also after a reset |
| Headers of a SPDY stream                                       | 32 in 8 KiB                                                           |
| Ports of the WebSocket port forward protocol                   | 40                                                                    |
| Connections which did not get upgraded yet                     | 256 since v1.2.1, see [Streaming security](#streaming-security)       |
| Time to send the request head, to get upgraded                 | 5 seconds, 10 seconds, see [Streaming security](#streaming-security)  |
| Request head                                                   | 64 KiB since v1.2.2, a larger one gets a 431                          |
| Exec output while the runtime starts the process               | 16 MiB per stream, the session fails above, see above                 |
| End of a session the client takes nothing of                   | 60 seconds or a longer exec or attach idle timeout, since v1.2.2      |
| Stdin before a clean close which the process takes none of     | 5 seconds since unreleased, about a second before                     |
| Idle connection                                                | The `idleTimeoutSec` of the request, see below                        |

`idleTimeoutSec` ends a connection which got no frame of the client and no
data either way for that long: an exec or attach session fails with an error
status and an exec process gets killed, port forwarding closes the forwarded
connection. The time the runtime takes to start an exec, WebSocket pings and
pongs (like the heartbeat of client-go), the pings of the server and the
answers to them and other control frames of the server do not count. A
stream which slowly takes its data keeps it active. SPDY clients like
client-go ping every few seconds, which counts like for CRI-O. The Go client
rounds its `IdleTimeout` up to whole seconds.

### Streaming protocols by version

The protocols the streaming server speaks and the first release with them:

| Operation    | Transport                           | Subprotocols                                                                                               | Since  |
| ------------ | ----------------------------------- | ---------------------------------------------------------------------------------------------------------- | ------ |
| Exec, attach | WebSocket                           | `v5.channel.k8s.io`                                                                                        | v0.7.0 |
| Exec, attach | WebSocket                           | `v4.channel.k8s.io`, `v4.base64.channel.k8s.io`, `channel.k8s.io`, `base64.channel.k8s.io`, no subprotocol | v1.2.0 |
| Exec, attach | SPDY/3.1                            | `v4.channel.k8s.io`, `v3.channel.k8s.io`, `v2.channel.k8s.io`, `channel.k8s.io`                            | v1.2.0 |
| Port forward | SPDY/3.1                            | `portforward.k8s.io`                                                                                       | v1.2.0 |
| Port forward | SPDY/3.1 tunneled through WebSocket | `SPDY/3.1+portforward.k8s.io`                                                                              | v1.2.0 |
| Port forward | WebSocket                           | `v4.channel.k8s.io`, `v4.base64.channel.k8s.io`, no subprotocol                                            | v1.2.0 |

From v0.7.0 to v1.0.1, `servePortForwardContainer` returns a URL, but the
server does not implement the protocol behind it. A connection to that URL
panics, and since v0.8.0 the release builds abort on a panic, so the server
exits and its containers lose their monitor. v1.1.0 and v1.1.1 reject the RPC
with an unimplemented error. The `idleTimeoutSec` of the three RPCs is new in
v1.2.0 as well: older servers ignore it for exec and attach, and do not serve
port forwarding at all.

### Streaming security

The streaming server speaks plain HTTP on `127.0.0.1`, without TLS or
authentication, so the token in the URL is the only access control. Every
process on the host which can reach the loopback interface of the network
namespace of the server may connect, the token keeps it from using a session
it did not get the URL of:

- A token is 128 random bits from the operating system, written as 32
  lowercase hex digits.
- A token is valid once, the first connection which the server accepts
  takes its session.
- A token which no client used expires after 60 seconds.
- The server keeps a bounded number of unused URLs per kind (exec and attach,
  port forward).
- A token only works on the path of its kind, `/exec/`, `/attach/` or
  `/portforward/`, an exec token on the attach path gets a 404 like an unknown
  one. Before v1.2.1, exec and attach tokens work on either of their paths.

Since v1.2.1, at most 256 connections which did not get upgraded yet get
served at once, more wait in the backlog of the listener. A connection serves a
single request, it has 5 seconds to send the head of the request and 10 seconds
until it got upgraded, otherwise the server closes it. A local process which
holds all of these connections open delays the requests of others by up to 5
seconds, as it has to send a request head within that time, but cannot use up
the file descriptors of the server. Since v1.2.2, the head of a request may
take at most 64 KiB, a larger one gets a 431, so that these connections hold
about 16 MiB at most.

The logs show only the first 8 hex digits of a token, which tell the sessions
apart but are not enough to use one. Responses never name the token: a request
for a token which is unknown, used or expired gets a bare 404, one whose token
is not 32 lowercase hex digits a 400 `Invalid URL`. Since v1.2.2, uppercase
hex digits get the 400 as well, before they matched the token like lowercase
ones.

## File descriptor socket

`startFdSocket` makes the server listen on a `SOCK_SEQPACKET` UNIX socket for
passing file descriptors, which the client can then reference by slot number in
`createContainer` (`additionalFds` and `leakFds`). A request carries the
descriptors as `SCM_RIGHTS` ancillary data and one little endian `u64`
`(request_id << 32) | num_fds`. The response repeats that `u64` followed by one
`u64` slot per descriptor. On error the lower 32 bits of the first word are
all ones and the rest of the message is the error text. A request carries at
most 253 descriptors, the most the kernel passes with one message
(`SCM_MAX_FD`). Received descriptors are closed when the connection ends or on
an empty request, a `u64` of zero without descriptors, which gets no response.
Since v1.2.1, a connection holds at most 1024 descriptors which no request took
yet, and all connections together a quarter of the open files limit of the
server, a request beyond that fails. The full description lives in the module
documentation of [fd_socket.rs](../conmon-rs/server/src/fd_socket.rs).

## Features

Beyond its version, `version` reports the optional features of a server,
which clients check before they rely on them. `conmonrs --version-json` prints
them as well, and so does `conmonrs --version` since v1.2.2:

| Feature              | Since  | What the server does                                                                                                                  |
| -------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------- |
| `sdnotify`           | v1.1.0 | Relays the `sd_notify` messages of a container to the `sdnotifySocket` of `createContainer`.                                          |
| `portforward`        | v1.2.0 | `servePortForwardContainer` forwards ports, see [Port forwarding](#port-forwarding).                                                  |
| `idletimeout`        | v1.2.0 | Exec and attach sessions apply their `idleTimeoutSec`, every server with `portforward` applies the one of port forwarding.            |
| `streamingprotocols` | v1.2.0 | Exec and attach sessions speak SPDY/3.1 and the WebSocket protocols before `v5.channel.k8s.io`.                                       |
| `journaldfields`     | v1.2.1 | The `journald` container log driver writes the `containerName`, `tag` and `labels` of the request, see [Logging](logging.md#logging). |

How to rely on them:

- Servers before v1.1.0 report an empty list, they do not know the field.
- Features only get added, a later release never drops one. A client
  ignores the ones it does not know.
- Check the features of the server the client talks to, not the ones of the
  binary on disk: a server keeps running across upgrades, see
  [Upgrading](cri-o.md#upgrading).
- Check them again after every reconnect, the server behind the socket may
  have been replaced by one of another version. The Go client records them
  on every connection, its methods which need a feature check it and fail
  with `ErrUnsupported`, `SupportsFeature` reports them to the caller.
