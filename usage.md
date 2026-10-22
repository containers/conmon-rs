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

CRI-O should now use conmon-rs after a restart, which is being indicated by the
debug logs when creating a container:

```text
…
DEBU[…] Using conmonrs version: …, tag: …, commit: …, build: …, \
            target: x86_64-unknown-linux-gnu, rustc …, cargo …  file="oci/runtime_pod.go:147"
…
```

CRI-O versions since v1.25 support conmon-rs, the `main` branches of both
projects are kept working together.

## Command line

The client starts the server with these flags, every flag can also be set
through the environment variable in parentheses:

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

When the server forks, the parent writes the child's PID to `pidfile` in the
runtime directory and exits. The server listens on `conmon.sock` in the runtime
directory, lowers its own OOM score to -1000 and acts as child subreaper for
the containers. Note that the OCI runtime inherits that OOM score, so clients
must set `process.oomScoreAdj` in their OCI configuration, like CRI-O does,
otherwise the container processes are not OOM killable.

### The `pause` subcommand

`conmonrs pause` creates the pod level namespaces. The server runs it for the
`createNamespaces` RPC, it is not meant to be used directly:

```console
> conmonrs pause --base-path /var/run --pod-id $POD_ID --ipc --net --pid --uts \
    [--user --uid-mappings "0 1000 1" --gid-mappings "0 1000 1"]
```

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
plain Go request and response structures.

### Attach socket

`attachContainer` makes the server listen on a `SOCK_SEQPACKET` UNIX socket at
the requested path. Packets from the client are the container's stdin, passed
on as they are, NUL bytes included. The client's input ends when it shuts down
the write side of its connection. Packets to the client start with one byte
selecting the stream, `2` for stdout and `3` for stderr, followed by up to 8191
bytes of data. A single `0` byte tells the client that the container output
ended. This is the same protocol conmon speaks, so existing attach clients
work unchanged.

### Streaming server

`serveExecContainer`, `serveAttachContainer` and `servePortForwardContainer`
return a URL on a server the conmonrs process starts on demand on `127.0.0.1`
with a random port. Each URL carries a one time token which is valid for a
bounded time. CRI-O passes these URLs to the kubelet, which connects directly.

Exec and attach speak the Kubernetes remote command protocols on the same
URL. Over WebSocket, `v5.channel.k8s.io` is what kubectl uses: the first byte
of each binary message is the stream (`0` stdin, `1` stdout, `2` stderr, `3`
errors, `4` terminal resize events as JSON, `255` close). The versions before
it (`v4.channel.k8s.io`, `channel.k8s.io` and their `base64` variants) frame the
messages the same way without the close signal, base64 ones as text messages
with the stream as an ASCII digit offset, and start with an empty message on
the first output stream. Like for the Kubernetes runtime streaming server, a
client without a subprotocol gets binary `channel.k8s.io`, and one which only
requests unsupported subprotocols gets a 403. Over SPDY/3.1, which the kubelet
uses towards the runtime also for WebSocket clients, the client opens a stream
per channel by the `streamType` header (`error`, `stdin`, `stdout`, `stderr`
and with a terminal `resize`, which may also come after the start), the end of
the stdin stream closes stdin. The server keeps its side of the stdin stream
open until the session ends, except for `channel.k8s.io`, like the kubelet.
Streams for stdout or stderr which come after the start end right away. The
server supports `v4.channel.k8s.io`, `v3`, `v2` and `channel.k8s.io`, the order
of the client decides. From v4 on, the final status is JSON, before only a
failure is reported as text. A client which does not take the end of a session
within a few seconds gets its connection closed. With `idleTimeoutSec`, a
session whose connection got no frame of the client and no data either way for
that long fails with an error status, an exec process gets killed then. The
time the runtime takes to start an exec, WebSocket pings and pongs (like the
heartbeat of client-go), the pings of the server and the answers to them and
other control frames of the server do not count. Stdin counts once the server
reads it. Stdin which backs up behind a process which does not read it stops
the server from reading the client, which may then go idle, like a spdystream
connection whose reader waits.

Port forwarding speaks the Kubernetes `portforward.k8s.io` protocol over
SPDY/3.1, which the kubelet uses towards the runtime also when the client
connected with a WebSocket. Per forwarded connection the client opens an
`error` and a `data` stream with the `port` and `requestID` headers. The
server connects to that port on `localhost` within the network namespace of
the request and copies between the connection and the data stream, a failure
is reported as text on the error stream. Only what the SPDY implementation of
Kubernetes uses is supported, there is no flow control: the server holds about
4 MiB of what the client sent on a connection, plus the frame it waits to
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
per port of the `port` query parameter. Port `i` gets the data channel `2i`
and the error channel `2i+1`, every message starts with its channel (an ASCII
digit offset in base64 text messages) and the first message of each channel
carries the port as a little endian `u16`. The session ends once all
connections ended.

`idleTimeoutSec` closes a connection which got no frame of the client and no
data either way for that long. A stream which slowly takes its data keeps it
active, WebSocket pings and pongs, the pings of the server and the answers to
them and other control frames of the server do not. SPDY clients like client-go
ping every few seconds, which counts like for CRI-O. Servers which support port
forwarding report the `portforward` feature in `version`.

### File descriptor socket

`startFdSocket` makes the server listen on a `SOCK_SEQPACKET` UNIX socket for
passing file descriptors, which the client can then reference by slot number in
`createContainer` (`additionalFds` and `leakFds`). A request carries the
descriptors as `SCM_RIGHTS` ancillary data and one little endian `u64`
`(request_id << 32) | num_fds`. The response repeats that `u64` followed by one
`u64` slot per descriptor. On error the lower 32 bits of the first word are
all ones and the rest of the message is the error text. Received descriptors
are closed when the connection ends or on an empty request. The full
description lives in the module documentation of
[fd_socket.rs](conmon-rs/server/src/fd_socket.rs).

## Logging

Container output goes to the log drivers requested per container. Log files
are truncated in place when they exceed the configured maximum size and on
`reopenLogContainer`, which CRI-O calls for log rotation.

- `containerRuntimeInterface`: the CRI log format, one line per entry with an
  RFC 3339 timestamp, the stream, a `P` (partial) or `F` (full) marker and the
  line.
- `json`: one JSON object per line with `timestamp` (seconds since the epoch),
  `pipe` (`stdout` or `stderr`), `message` (the line without its ending) and
  `partial: true` when the line had no ending yet.
- `journald`: one journal entry per line with the fields `CONTAINER_ID_FULL`,
  `CONTAINER_ID` (the first 12 characters) and `SYSLOG_IDENTIFIER=conmonrs`,
  at priority info for stdout and error for stderr, so that
  `journalctl CONTAINER_ID=<id>` shows the output like with conmon. A line
  which had no ending yet carries `CONTAINER_PARTIAL_MESSAGE=true`. The
  maximum size does not apply.

The JSON and journald drivers collect whole lines across reads, so a line
written in several pieces or a multi byte character split by a read still
ends up as one entry. Like the `P` marker of the CRI log, a partial entry is
a line which had no ending yet: it grew beyond 16 KiB, the output paused for
half a second, or the output ended without one.

The server's own logs go to the `--log-driver`. Running with
`--log-level debug` prints every RPC and I/O decision and is the first thing to
try when debugging a container that misbehaves under conmon-rs.

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
