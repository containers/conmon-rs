# Troubleshooting and tracing

## Troubleshooting

| Symptom                                                                       | Cause                                                                                                                                                                      |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| New pods fail, the server exits with `unexpected argument '--cgroup-manager'` | CRI-O v1.36 or older with a server of v1.0.0 or newer, see [Compatibility](cri-o.md#compatibility).                                                                        |
| CRI-O logs `Container monitor of ... does not support ...`                    | The server of the pod predates the feature, for example one kept across an upgrade, see [Upgrading](cri-o.md#upgrading).                                                   |
| A server refuses to start with `socket ... is in use by a running server`     | Another server serves the runtime directory, see below.                                                                                                                    |
| `client.New` fails with `ErrServerNotResponding`                              | A server accepts connections but does not answer, see below.                                                                                                               |
| `client.New` fails with `ErrServerExited`, the server has no logs             | The server log driver is `none`, the default of CRI-O. A crash leaves a core dump without symbols, since v1.2.2 a panic also leaves its message in the journal, see below. |
| Container output is missing in the journal                                    | journald did not keep up, see below.                                                                                                                                       |
| An attach client misses output                                                | It fell behind the container output, which the server logs as a warning, see [Attach socket](protocols.md#attach-socket).                                                  |

With the `systemd` server log driver, the default of `conmonrs`, the server's
own logs go to the journal, since v1.2.1 with the `SYSLOG_IDENTIFIER`
`conmonrs`, before only under its process name (`_COMM=conmonrs`). CRI-O
passes its `log_level` on to the servers, but since v1.34 starts them with the
server log driver `none`, unless the `monitor_env` of the runtime handler
contains `LOG_DRIVER=systemd`. A server with `none` writes no logs (since
v1.2.2, except for the message of a panic), the forked server has `/dev/null`
as its stdin, stdout and stderr. The container output only goes to the journal
with the `journald` container log driver, sent by the same process:

```console
> journalctl -t conmonrs
> journalctl _COMM=conmonrs
> journalctl CONTAINER_ID_FULL=<id>
```

The first command shows the logs of the server, before v1.2.2 together
with the output of the containers which use the `journald` container log
driver without a `tag`, the second one also the logs of older servers and the
output of all containers which use it. The third one shows only the output of
one container.

The server never waits for the journal with its own entries, they get lost while
journald does not keep up, for example while it restarts. The lines of container
output which the journal does not take right away wait up to about two seconds
together, the rest of what the server read gets dropped after that, which the
server logs as a warning, once until the journal takes lines again. Since
v1.2.2, the following chunks do not wait then, what the journal does not
take right away gets dropped for at least a second and after that as long as
it takes nothing, so that a stalled journal does not slow down the container
and its other logs, and one which does not keep up by about half at most. The
log files of the `containerRuntimeInterface` and `json` drivers lose nothing
this way.

The server runs crun with `--log=journald:<container ID>`, so what crun logs
goes to the journal, where `journalctl -t crun ID=<container ID>` shows it: its
errors, at the server log levels `warn`, `debug` and `trace` its warnings and
at `debug` and `trace` its debug messages, at `trace` since v1.2.2, before
only its errors. crun copies its errors to its stderr as well, which ends up in
the error of the failed call.

To see what the server does without an engine, run it in the foreground with
its logs on stdout and talk to its socket with a client:

```console
> conmonrs --runtime /usr/bin/crun --runtime-dir /tmp/conmonrs --skip-fork -d stdout -l debug
```

A server which fails before it forks, for example because the runtime does not
exist, makes `client.New` fail with what the server wrote to stderr. With the
`stdout` server log driver, also when `CONMON_LOG_DRIVER` in the `Env` of the
configuration selects it, the `Stderr` writer gets that output instead. A
server which exits after it forked, before it accepted connections, makes
`client.New` fail with an error matching `client.ErrServerExited`, only the
logs of a server log driver other than `none` tell why. The release builds
abort on a panic and leave a core dump. The panic message goes to stderr,
which is `/dev/null` unless the server log driver is `stdout`, since v1.2.2
with the other drivers to the journal as well (`journalctl -t conmonrs -p err`).
Since v1.2.2, a server which did not fork (`--skip-fork`) writes it only to
the stderr it kept, with the default panic hook. On hosts with systemd-coredump,
`coredumpctl list conmonrs` lists the crashes and `coredumpctl info <pid>`
shows the stack of each. The release builds are stripped, and the static
binaries have no unwind tables for the code of conmon-rs, so the backtrace has
no symbols and may stop early: the panic message in the journal is the main
clue. A usable backtrace needs a server built with
`CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.

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
[contrib/tracing](../contrib/tracing) runs a local Jaeger to look at the traces.
