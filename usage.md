# Usage

conmon-rs is started by a container engine, not by hand. The engine runs the
`conmonrs` binary once per pod, the binary forks into the background and the
engine talks to it over a UNIX domain socket. The Go client in
[pkg/client](pkg/client) does this for [CRI-O](https://github.com/cri-o/cri-o)
and is the reference for other clients.

## Configure CRI-O

CRI-O needs to be configured to use conmon-rs. To do this, change the runtime
configurations `runtime_type` and optionally the `monitor_path`, for example:

```console
> cat /etc/crio/crio.conf.d/99-runtimes.conf
```

```toml
[crio.runtime]
default_runtime = "crun"

[crio.runtime.runtimes.crun]
runtime_type = "pod"
monitor_path = "/path/to/conmonrs"  # Optional, lookup $PATH if not set
```

The `pod` runtime type is what selects conmon-rs. A `monitor_path` pointing to
conmonrs with the default `oci` runtime type does not work, CRI-O expects conmon
there and rejects the configuration.

CRI-O should now use conmon-rs after a restart, which the CRI-O debug logs show
when it creates a container:

```text
…
DEBU[…] Using conmonrs version: …, tag: …, commit: …, build: …, \
            target: x86_64-unknown-linux-gnu, rustc …, cargo …  file="oci/runtime_pod.go"
…
```

CRI-O versions since v1.25 support conmon-rs, the `main` branches of both
projects are kept working together.

## Command line

The server takes these flags, every flag can also be set through the
environment variable in parentheses:

| Flag                              | Environment               | Default                 | Purpose                                                                            |
| --------------------------------- | ------------------------- | ----------------------- | ---------------------------------------------------------------------------------- |
| `-r, --runtime <RUNTIME>`         | `CONMON_RUNTIME`          |                         | Binary path of the OCI runtime, required for running the server.                   |
| `--runtime-dir <RUNTIME_DIR>`     | `CONMON_RUNTIME_DIR`      |                         | Directory for the sockets and other files the server creates at runtime, required. |
| `--runtime-root <RUNTIME_ROOT>`   | `CONMON_RUNTIME_ROOT`     |                         | Root directory passed to the OCI runtime (`--root`).                               |
| `-l, --log-level <LEVEL>`         | `CONMON_LOG_LEVEL`        | `info`                  | `trace`, `debug`, `info`, `warn`, `error` or `off`.                                |
| `-d, --log-driver <DRIVER>`       | `CONMON_LOG_DRIVER`       | `systemd`               | Where the server logs to: `systemd` (journald), `stdout` or `none`.                |
| `--skip-fork`                     | `CONMON_SKIP_FORK`        | `false`                 | Stay in the foreground instead of forking into the background (`1` or `true`).     |
| `-e, --enable-tracing`            | `CONMON_ENABLE_TRACING`   | `false`                 | Export OpenTelemetry traces (needs a build with the `tracing` feature).            |
| `-t, --tracing-endpoint <URL>`    | `CONMON_TRACING_ENDPOINT` | `http://127.0.0.1:4317` | OpenTelemetry gRPC endpoint.                                                       |
| `-v, --version [full]`            | `CONMON_VERSION`          |                         | Print the version, `full` includes the dependency tree.                            |
| `-j, --version-json, --vj [full]` | `CONMON_VERSION_JSON`     |                         | Print the version as JSON.                                                         |

Before it forks, the server sets its umask to 022 and removes `NOTIFY_SOCKET`
from its environment, so that the runtime, the cleanup commands and the pause
process do not get the sd_notify socket of the caller. It lowers its own OOM
score to -1000, turns transparent huge pages off for itself and raises its soft
limit of open files to the hard one. The OCI runtime gets the OOM score, the
huge pages setting and the open files limits the server started with, so the
container processes get the OOM score of the client unless it sets
`process.oomScoreAdj` in the OCI configuration, like CRI-O does.

When the server forks, the parent writes the child's PID to `pidfile` in the
runtime directory and exits. With a log driver other than `stdout`, the child
replaces its stdin, stdout and stderr with `/dev/null` like a daemon, so neither
the server nor the processes it starts write to the ones of the caller anymore.
The server closes the other file descriptors it inherited, changes into `/`,
acts as child subreaper for the containers and listens on `conmon.sock` in the
runtime directory.

The server adds arguments of its own to every runtime call for a container or
an exec, before the arguments of the request: `--root=<RUNTIME_ROOT>`
for `--runtime-root`, and `--systemd-cgroup` for the `systemd` cgroup manager
of the request. If the file name of the runtime is `crun`, it also adds
`--log=journald:<container ID>`, and for the log levels `debug`, `warn` and
`error` of the server `--log-level=debug`, `--log-level=warning` or
`--log-level=error`, crun keeps its default for the other levels.

On SIGTERM or SIGINT, the server kills the containers, stops the pause process
and removes its sockets within 9.5 seconds, before `Shutdown` of the Go client
gives up after 10 with `ErrShutdownTimeout`. If any of that fails or does not
finish in time, the server logs it as an error and exits with status 1. A
server which cannot listen for these signals does the same cleanup right away
and exits with status 1.

### The `pause` subcommand

`conmonrs pause` creates the pod level namespaces. The server runs it for the
`createNamespaces` RPC, it is not meant to be used directly:

```console
> conmonrs pause --base-path /var/run --pod-id $POD_ID --ipc --net --pid --uts \
    [--user --uid-mappings "0 1000 1" --gid-mappings "0 1000 1"]
```

`-p, --base-path` defaults to `/var/run` and can also be set with
`CONMON_PAUSE_BASE_PATH`, `--pod-id` with `CONMON_PAUSE_POD_ID`. With `--user`,
`-u, --uid-mappings` and `-g, --gid-mappings` are required. Each takes one
mapping in the format `CONTAINER_ID HOST_ID SIZE` and can be repeated for more
of them.

Each requested namespace is unshared and bind mounted to
`$BASE_PATH/[ipc,pid,net,user,uts]ns/$POD_ID`. The bind mounts keep the
namespaces alive, except for a PID namespace, which dies with its init process.
Only when `--pid` is requested a pause process stays behind, its PID is written
to `$BASE_PATH/conmonrs/$POD_ID.pid`. The mounts and the pause process are
removed when the server shuts down.

## Protocols

### Cap'n Proto RPC

All operations are RPCs on the `conmon.sock` UNIX domain socket, defined in
[conmon-rs/server/proto/conmon.capnp](conmon-rs/server/proto/conmon.capnp):
`version`, `createContainer`, `execSyncContainer`, `attachContainer`,
`reopenLogContainer`, `setWindowSizeContainer`, `createNamespaces`,
`startFdSocket`, `serveExecContainer`, `serveAttachContainer` and
`servePortForwardContainer`. The Go client wraps each of them in a method with
plain Go request and response structures. An OCI runtime which hangs gets
killed and its call fails: for `createContainer` after 10 minutes, for an exec
60 seconds after its timeout, or after 60 seconds without one, as the runtime
only starts the exec process. Each runtime runs in a process group of its own,
which gets killed with it, so the helpers it started do not stay behind.

### Attach socket

`attachContainer` makes the server listen on a `SOCK_SEQPACKET` UNIX socket at
the requested path, which has to be absolute since unreleased. Packets from the
client are the container's stdin, passed on as they are, NUL bytes included. A
packet of more than 64 KiB gets truncated to its first 64 KiB. The Go client
sends one read of its stdin per packet, at most 32 KiB plus the start of a
detach key sequence which it held back. The client's input ends when it shuts
down the write side of its connection. Packets to the client start with one byte
selecting the stream, `2` for stdout and `3` for stderr, followed by up to 8191
bytes of data. A single `0` byte tells the client that the container output
ended. This is the same protocol conmon speaks, so existing attach clients
work unchanged.

With `stopAfterStdinEof`, the server closes the container's stdin once the
input of a client ended, the output is still forwarded until the container
closes it. The stdin of a terminal is never closed. The value of the latest
attach call of a container applies to the connections the server accepts
afterwards on any of its attach sockets, so all attach calls of a container
should agree on it, CRI-O derives it from the container configuration.

The container output never waits for an attach client. A client which falls
more than 100 reads of the output behind, of up to 8 KiB each, about 800 KiB,
skips the oldest output, which the server logs as a warning. The same applies
to the attach sessions of the streaming server.

### Streaming server

`serveExecContainer`, `serveAttachContainer` and `servePortForwardContainer`
return a URL on a server the conmonrs process starts on demand on `127.0.0.1`
with a random port. Each URL carries a one time token which is valid for a
minute. At most 1000 URLs of each kind (exec and attach, port forward) wait for
their client at once, a request beyond that fails with an error starting with
`maximum number of in-flight requests exceeded` (`ErrTooManyRequests` of the Go
client), like the kubelet fails requests beyond its limit. v1.1.1 and older drop
the oldest URL instead. CRI-O passes these URLs to the kubelet, which connects
directly.

Exec and attach speak the Kubernetes remote command protocols on the same
URL. Over WebSocket, `v5.channel.k8s.io` is what kubectl uses: the first byte
of each binary message is the stream (`0` stdin, `1` stdout, `2` stderr, `3`
errors, `4` terminal resize events as JSON, `255` close). The versions before
it (`v4.channel.k8s.io`, `channel.k8s.io` and their `base64` variants) frame the
messages the same way without the close signal, base64 ones as text messages
with the stream as an ASCII digit offset, and start with an empty message on
the first output stream. Like for the Kubernetes runtime streaming server, a
client without a subprotocol gets binary `channel.k8s.io`, and one which only
requests unsupported subprotocols gets a 403. A WebSocket message or frame of
the client may carry at most 1 MiB, also for port forwarding, a larger one ends
the connection. Over SPDY/3.1, which the kubelet uses towards the runtime also
for WebSocket clients, the client opens a stream per channel by the
`streamType` header (`error`, `stdin`, `stdout`, `stderr` and with a terminal
`resize`, which may also come after the start), the end of the stdin stream
closes stdin. The server keeps its side of the stdin stream open until the
session ends, except for `channel.k8s.io`, like the kubelet.
Streams for stdout or stderr which come after the start end right away. The
server supports `v4.channel.k8s.io`, `v3`, `v2` and `channel.k8s.io`, the order
of the client decides. From v4 on, the final status is JSON, before only a
failure is reported as text. Since unreleased, a client which takes the end of a
session slowly, the output queued for it, the status and the close, gets all of
it, only one which takes nothing of it for 60 seconds gets its connection
closed. Before, a client which did not take all of it within a few seconds lost
the rest, and client-go took the missing status for a success. After the close,
the server reads and drops what the client still sends until it answered the
WebSocket close or closed the SPDY connection, for up to 5 seconds, as closing a
connection with unread input resets it, which may lose the end of the session.
Port forwarding does the same. An exec process which writes more than 16 MiB to
stdout or stderr while the runtime still starts it gets its output cut there,
the session then fails with an error status instead of the exit code since
unreleased. With `idleTimeoutSec`, a session whose connection got no frame of
the client and no data either way for that long fails with an error status, an
exec process gets killed then. The time the runtime takes to start an exec,
WebSocket pings and pongs (like the heartbeat of client-go), the pings of the
server and the answers to them and other control frames of the server do not
count. Stdin counts once the server reads it. Stdin which backs up behind a
process which does not read it stops the server from reading the client, which
may then go idle, like a spdystream connection whose reader waits. A session
holds at most about 3 MiB of stdin which the process did not take yet, on top of
what a SPDY connection holds (see port forwarding below), empty stdin messages
count as nothing.

Port forwarding speaks the Kubernetes `portforward.k8s.io` protocol over
SPDY/3.1, which the kubelet uses towards the runtime also when the client
connected with a WebSocket. Per forwarded connection the client opens an
`error` and a `data` stream with the `port` and `requestID` headers. The
server connects to that port on `localhost` within the network namespace of
the request, whose `netNsPath` has to be absolute since unreleased, and copies
between the connection and the data stream, a failure is reported as text on
the error stream, what the client sends on it gets dropped. Only what the SPDY
implementation of Kubernetes uses is supported: a connection has at most 1024
streams, which count until the server let go of them, also after a reset, and a
stream at most 32 headers in 8 KiB. There is no flow control: the server holds
about 4 MiB of what the client sent on a connection, plus the frame it waits to
place, and stops reading it until the streams took their data. What a
forwarded connection sends while it gets established, up to 1 MiB, moves into a
connect budget of 16 MiB per session instead, so that a port which does not
answer does not stop the other connections. Once that is used up, connecting
streams stop taking their data like any stream which does not take it. Any
number of connections get established at once. Clients which connect directly,
like crictl with the WebSocket transport, may tunnel the SPDY connection through
binary WebSocket messages with the `SPDY/3.1+portforward.k8s.io` subprotocol.

The URL also serves the older WebSocket channel protocol (`v4.channel.k8s.io`,
`v4.base64.channel.k8s.io` or no subprotocol), which forwards one connection
per port of the `port` query parameter, at most 40 ports, a request with more
gets a 400. Port `i` gets the data channel `2i` and the error channel `2i+1`,
every message starts with its channel (an ASCII digit offset in base64 text
messages) and the first message of each channel carries the port as a little
endian `u16`. The session ends once all connections ended.

`idleTimeoutSec` closes a connection which got no frame of the client and no
data either way for that long. A stream which slowly takes its data keeps it
active, WebSocket pings and pongs, the pings of the server and the answers to
them and other control frames of the server do not. SPDY clients like client-go
ping every few seconds, which counts like for CRI-O.

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

Beyond its version, `version` reports the optional features of a server, which
clients check before they rely on them:

- `sdnotify`: the server relays the `sd_notify` messages of a container to
  the `sdnotifySocket` of `createContainer`.
- `portforward`: `servePortForwardContainer` forwards ports with the protocols
  above.
- `idletimeout`: exec and attach sessions apply their `idleTimeoutSec`, every
  server with `portforward` applies the one of port forwarding.
- `streamingprotocols`: exec and attach sessions speak SPDY/3.1 and the
  WebSocket protocols before `v5.channel.k8s.io`.
- `journaldfields`: the `journald` log driver writes its `containerName`,
  `tag` and `labels`, see [Logging](#logging).

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
the file descriptors of the server. Since unreleased, the head of a request may
take at most 64 KiB, a larger one gets a 431, so that these connections hold
about 16 MiB at most.

The logs show only the first 8 hex digits of a token, which tell the sessions
apart but are not enough to use one. Responses never name the token: a request
for a token which is unknown, used or expired gets a bare 404, one whose token
is not 32 hex digits a 400 `Invalid URL`.

### File descriptor socket

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
documentation of [fd_socket.rs](conmon-rs/server/src/fd_socket.rs).

## Logging

Container output goes to the log drivers requested per container. When the next
entry would exceed the configured maximum size, the server writes the entries
read before it to the old file and replaces the log file with a new empty one,
like conmon: it creates `.<name>.conmonrs-tmp` next to it, a name which the log
cleanup of the kubelet does not match, and renames it over the path, so that a
reader which follows the log, like `kubectl logs -f`, reads the old file to its
end first. `reopenLogContainer`, which CRI-O calls after the kubelet rotated the
log, starts a new file at the path the same way. The server appends to the log
files, so a file which gets truncated in place, for example to free disk space,
continues at its new end. Since unreleased, a log file which exists when the
container gets created is appended to like with conmon instead of truncated, its
content counts for the maximum size. A write which fails in the middle of an
entry, like on a full disk, drops the rest of it, the next entry starts on a
line of its own, and so does the first entry after existing content without a
final newline. A rotation which fails drops the entries which would exceed the
size, the next read of output tries it again.

- `containerRuntimeInterface`: the CRI log format, one line per entry with an
  RFC 3339 timestamp in nanoseconds of when the server read the output, the
  stream, a `P` (partial) or `F` (full) marker and the line.
- `json`: one JSON object per line with `timestamp` (seconds since the epoch),
  `pipe` (`stdout` or `stderr`), `message` (the line without its ending) and
  `partial: true` when the line had no ending yet.
- `journald`: one journal entry per line with the fields `CONTAINER_ID_FULL`,
  `CONTAINER_ID` (the first 12 characters) and `SYSLOG_IDENTIFIER=conmonrs`,
  at priority info for stdout and error for stderr, so that
  `journalctl CONTAINER_ID_FULL=<id>` or `CONTAINER_ID=<short id>` shows the
  output like with conmon. A line which had no ending yet carries
  `CONTAINER_PARTIAL_MESSAGE=true`. The maximum size does not apply. Like the
  options of conmon, `containerName` adds `CONTAINER_NAME`, `tag` adds
  `CONTAINER_TAG` and replaces the `SYSLOG_IDENTIFIER`, and `labels` add
  fields of their own, whose names follow the rules of the journal: uppercase
  letters, digits and underscores, starting with a letter, at most 64
  characters. A container takes at most 256 labels with 64 KiB of names and
  values together, journald drops entries with more than 1024 fields.

The JSON and journald drivers collect whole lines across reads, so a line
written in several pieces or a multi byte character split by a read still
ends up as one entry. Like the `P` marker of the CRI log, a partial entry is
a line which had no ending yet: it grew beyond 16 KiB, the output paused for
half a second, or the output ended without one.

The server's own logs go to the `--log-driver`. Running with
`--log-level debug` prints every RPC and I/O decision and is the first thing to
try when debugging a container that misbehaves under conmon-rs.

## Troubleshooting

With the `systemd` log driver, the default of `conmonrs`, the server's own
logs go to the journal, since v1.2.1 with the `SYSLOG_IDENTIFIER` `conmonrs`,
before only under its process name (`_COMM=conmonrs`). CRI-O passes
its `log_level` on to the servers, but since v1.34 starts them with the log
driver `none`, unless the `monitor_env` of the runtime handler contains
`LOG_DRIVER=systemd`. The container output only goes to the journal with the
`journald` log driver of the container, sent by the same process:

```console
> journalctl -t conmonrs
> journalctl _COMM=conmonrs
> journalctl CONTAINER_ID_FULL=<id>
```

The first command shows the logs of the server together with the output of
the containers which use the `journald` log driver without a `tag`, the
second one also the logs of older servers and the output of containers with a
`tag`. The third one shows only the output of one container.

The server never waits for the journal with its own entries, they get lost while
journald does not keep up, for example while it restarts. The lines of container
output which the journal does not take right away wait up to about two seconds
together, the rest of what the server read gets dropped after that, which the
server logs as a warning, once until the journal takes lines again. The log
files of the `containerRuntimeInterface` and `json` drivers lose nothing this
way.

To see what the server does without an engine, run it in the foreground with
its logs on stdout and talk to its socket with a client:

```console
> conmonrs --runtime /usr/bin/crun --runtime-dir /tmp/conmonrs --skip-fork -d stdout -l debug
```

A server which fails before it forks, for example because the runtime does not
exist, makes `client.New` fail with what the server wrote to stderr. With the
`stdout` log driver, also when `CONMON_LOG_DRIVER` in the `Env` of the
configuration selects it, the `Stderr` writer gets that output instead.

Only one server runs per runtime directory. A server refuses to start with
`socket ... is in use by a running server` while another one serves
`conmon.sock` there, a socket which a server left behind when it exited gets
replaced. The Go client connects to such a running server instead of starting
a new one. If that server accepts the connection but does not answer within a
few seconds, `client.New` returns an error matching
`client.ErrServerNotResponding` which names its PID. The client does not
start a second server then, as it would take over the sockets and orphan the
containers. Collect the logs of that server, then kill it, the next
`client.New` starts a new one. The containers of the pod lose their monitor
that way and should be recreated.

To profile the memory of a server, the Go client attaches
[heaptrack](https://github.com/KDE/heaptrack) to a server which `client.New`
starts if its configuration has `Heaptrack: &client.Heaptrack{Enabled: true}`.
It runs `heaptrack --record-only -p <pid>` from the `BinaryPath` of the
configuration or the `PATH`, as a child of the client process, with
`-o <OutputPath>` if that is set, otherwise heaptrack writes the profile to the
working directory of the client under its default name. A server which already
runs does not get profiled. CRI-O enables it with `HEAPTRACK_OUTPUT_PATH` or
`HEAPTRACK_BINARY_PATH` in the `monitor_env` of the runtime handler. With
`HEAPTRACK_OUTPUT_PATH`, the profiles end up below that path as
`cri-o.conmon-rs.<container id>`, named after the container whose creation
started the server, with only `HEAPTRACK_BINARY_PATH` in the working directory
of CRI-O.

## Tracing

The server can export OpenTelemetry traces of its RPCs over OTLP gRPC. This
needs a build with the `tracing` feature:

```console
> cargo build --release --features tracing
```

The static release binaries and the RPMs get built without it, they accept
`--enable-tracing` and `--tracing-endpoint` and ignore them. With the feature,
`--enable-tracing` exports to `--tracing-endpoint`, `http://127.0.0.1:4317` by
default. The Go client starts the server with these flags if its
configuration has `Tracing: &client.Tracing{Enabled: true}`. It then passes the
span of the context of a call along with the RPC, if there is one, through the
global propagator of OpenTelemetry (`otel.SetTextMapPropagator`), which does
nothing unless the caller registers one, like CRI-O does. The server reads the
W3C Trace Context, so the propagator has to write it, like
`propagation.TraceContext{}`.
[contrib/tracing](contrib/tracing) runs a local Jaeger to look at the traces.

## Red Hat OpenShift

OpenShift ships conmon-rs. To use it for a runtime, apply a `MachineConfig`
that sets the runtime type, for example for `crun`:

```yaml
apiVersion: machineconfiguration.openshift.io/v1
kind: MachineConfig
metadata:
  labels:
    machineconfiguration.openshift.io/role: worker
  name: 99-worker-conmonrs
spec:
  config:
    ignition:
      version: 3.2.0
    storage:
      files:
        - contents:
            source: data:,%5Bcrio.runtime.runtimes.crun%5D%0Aruntime_type%20%3D%20%22pod%22%0A
          mode: 420
          overwrite: true
          path: /etc/crio/crio.conf.d/99-conmonrs.conf
```

The same can be done for the `master` role or any other configured runtime.
Builds of every `main` commit are available in the [podman-next
copr](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs)
and can be installed on RHCOS with `rpm-ostree override replace <rpm url>`.
