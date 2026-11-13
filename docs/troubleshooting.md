# Troubleshooting and tracing

## Troubleshooting

| Symptom                                                                       | Cause                                                                                                                                                                                   |
| ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| New pods fail, the server exits with `unexpected argument '--cgroup-manager'` | CRI-O v1.36 or older with a server of v1.0.0 or newer, see [Compatibility](cri-o.md#compatibility).                                                                                     |
| CRI-O logs `Container monitor of ... does not support ...`                    | The server of the pod predates the feature, for example one kept across an upgrade, see [Upgrading](cri-o.md#upgrading).                                                                |
| A server refuses to start with `socket ... is in use by a running server`     | Another server serves the runtime directory, see [One server per runtime directory](#one-server-per-runtime-directory).                                                                 |
| `client.New` fails with `ErrServerNotResponding`                              | A server accepts connections but does not answer, see [One server per runtime directory](#one-server-per-runtime-directory).                                                            |
| `client.New` fails with `ErrServerExited`, the server has no logs             | The server log driver is `none`, the default of CRI-O, see [Server logs](#server-logs). A crash leaves a core dump without symbols and the message of a panic, see [Crashes](#crashes). |
| Container output is missing in the journal                                    | journald did not keep up, see [Journal stalls](#journal-stalls).                                                                                                                        |
| An attach client misses output                                                | It fell behind the container output, which the server logs as a warning, see [Attach socket](protocols.md#attach-socket).                                                               |
| A pod misbehaves under CRI-O and its server logs nothing                      | CRI-O starts the servers with the log driver `none` and its own log level, which a running server keeps, see [Debugging a pod](#debugging-a-pod).                                       |

### Server logs

With the `systemd` server log driver, the default of `conmonrs`, the server's
own logs go to the journal, since v1.2.1 with the `SYSLOG_IDENTIFIER`
`conmonrs`. With the server log driver `none`, the default of CRI-O, a server
writes no logs, except for the message of a panic since v1.2.2, see
[Crashes](#crashes). The forked server then has `/dev/null` as its stdin,
stdout and stderr. The container output only goes to the journal with the
`journald` container log driver, sent by the same process:

```console
> journalctl -t conmonrs
> journalctl _COMM=conmonrs
> journalctl CONTAINER_ID_FULL=<id>
```

The first command shows the logs of the server, the second one also the logs
of servers before v1.2.1 and the output of all containers which use the
`journald` container log driver. The third one shows only the output of one
container.

A line of the server log has the level, the span of the request or session it
belongs to, the module and line of the source and the message. With the
`stdout` driver the time comes first, the journal has its own:

```text
 INFO create_container{container_id="<id>"}: conmonrs::child_reaper::watcher: <line>: Container exited pid=<pid> exit_code=0 oomed=false
 WARN exec_sync_container{container_id="<id>"}: conmonrs::rpc: <line>: Unable to start exec: runtime exited with status 1: ...
```

Since unreleased, the span names the container at every log level, also for
what the server logs about it later, like its exit under the span of the
`create_container` request which started it. `journalctl -t conmonrs -g <id>`
finds the lines of one container. The lines of a streaming session have a
`session{container_id="<id>"}` span. At the `debug` level the spans carry more,
like the ID of the request and the PID of the process.

At the default level `info`, the server logs its start and shutdown, the exit
of each container with its PID, its exit code and whether it got OOM killed,
an OOM as it happens, a runtime or an exec process which got killed at its
timeout and the start of each exec and attach session. Warnings and errors
tell what went wrong for a container or a client, like an exec which could not
start, output which got dropped or an exit file which could not be written.
The lines of the shutdown name the containers which did not exit by their
PIDs. A successful `execSyncContainer`, like a probe, logs nothing at `info`.

On a host without journald, the `systemd` driver logs nothing and reports no
error, like `sd_journal_send`. Use the `stdout` driver there.

To see what the server does without an engine, run it in the foreground with
its logs on stdout and talk to its socket with a client:

```console
> conmonrs --runtime /usr/bin/crun --runtime-dir /tmp/conmonrs --skip-fork -d stdout -l debug
```

### Journal stalls

The server never waits for the journal with its own entries, they get lost while
journald does not keep up, for example while it restarts. The lines of container
output which the journal does not take right away wait up to about two seconds
together, the rest of what the server read gets dropped after that, which the
server logs as a warning, once until the journal takes lines again. The
following chunks do not wait then, what the journal does not take right away
gets dropped for at least a second and after that as long as it takes nothing,
so that a stalled journal does not slow down the container and its other logs,
and one which does not keep up by about half at most. The log files of the
`containerRuntimeInterface` driver lose nothing this way.

### crun logs

The server runs crun with `--log=journald:<container ID>`, whatever its own
log driver, so what crun logs goes to the journal, where
`journalctl -t crun ID=<container ID>` shows it: its errors, at the server log
levels `warn`, `debug` and `trace` its warnings and at `debug` and `trace` its
debug messages. crun copies its errors to its stderr as well, which ends up in
the error of the failed call.

### Crashes

A server which fails before it forks, for example because the runtime does not
exist, makes `client.New` fail with what the server wrote to stderr. With the
`stdout` server log driver, also when `CONMON_LOG_DRIVER` in the `Env` of the
configuration selects it, the `Stderr` file gets that output instead. A
server which exits after it forked, before it accepted connections, makes
`client.New` fail with an error matching `client.ErrServerExited`, only the
logs of a server log driver other than `none` tell why.

The release builds abort on a panic and leave a core dump. The panic message
goes to stderr, which is `/dev/null` unless the server log driver is `stdout`,
and since v1.2.2 with the other drivers, `none` included, to the journal as well
(`journalctl -t conmonrs -p err`). A server which did not fork (`--skip-fork`)
writes it only to the stderr it kept, with the default panic hook. On hosts with
systemd-coredump, `coredumpctl list conmonrs` lists the crashes and `coredumpctl
info <pid>` shows the stack of each. The release builds are stripped, and the
static binaries have no unwind tables for the code of conmon-rs, so the
backtrace has no symbols and may stop early: the panic message in the journal is
the main clue. A usable backtrace needs a server built with
`CARGO_PROFILE_RELEASE_STRIP=false
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.

### One server per runtime directory

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

### Memory profiling

To profile the memory of a server, the Go client attaches
[heaptrack](https://github.com/KDE/heaptrack) to a server which `client.New`
starts if its configuration has `Heaptrack: client.Heaptrack{Enabled: true}`.
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

## Debugging a pod

CRI-O starts the server of a pod when it creates the infra container of the pod,
whose ID is the ID of the pod. The runtime directory of the server holds the RPC
socket `conmon.sock`. CRI-O drops the infra container of a pod by default
(`drop_infra_ctr = true`), unless the pod shares its PID namespace, the runtime
directory is then
`/run/containers/storage/overlay-containers/<pod ID>/userdata`. With an infra
container which runs, it is the directory of that container in the storage,
`/var/lib/containers/storage/overlay-containers/<pod ID>/userdata` by default,
whose `pidfile` belongs to the infra container: the client removes the one of
the server once it read it. The command line of the server names the runtime
directory, so the pod ID finds its process, along with the log level and driver
it runs with:

```console
> pgrep -af conmonrs | grep <pod ID>
… conmonrs --runtime /usr/bin/crun \
    --runtime-dir /run/containers/storage/overlay-containers/<pod ID>/userdata \
    --runtime-root /run/crun --log-level info --log-driver none
```

The level and the driver are the ones CRI-O had configured when it started the
server, a running server keeps them: no RPC or signal changes them. To get the
debug logs of a pod, configure CRI-O so that new servers log to the journal at
the debug level, restart or reload (SIGHUP) CRI-O, and recreate the pod:

```toml
[crio.runtime]
log_level = "debug"

[crio.runtime.runtimes.crun]
runtime_path = "/usr/bin/crun"
runtime_type = "pod"
runtime_root = "/run/crun"
monitor_path = "/usr/bin/conmonrs"
monitor_env = ["LOG_DRIVER=systemd"]
```

The table of the runtime handler has to define the complete handler, see
[Configure CRI-O](cri-o.md#configure-cri-o).

CRI-O passes its own `log_level` on as the server log level (`fatal` and `panic`
as `off`). A restart and a reload (SIGHUP) of CRI-O both apply `log_level` and
the runtime handlers, `monitor_env` included, to the servers it starts
afterwards. Without `LOG_DRIVER` in the `monitor_env` of the runtime handler,
CRI-O starts the servers with the log driver `none`, see [Server
logs](#server-logs). The debug logs then show every RPC and I/O decision of the
server under `journalctl -t conmonrs`, together with those of the other pods.
The [crun logs](#crun-logs) reach the journal with any server log driver.

CRI-O moves the server into the `monitor_cgroup` of the runtime handler, with
the systemd cgroup manager into the scope `crio-conmon-<pod ID>.scope` of the
slice that names, `system.slice` by default, or of the slice of the pod with
`pod`. `cat /proc/<pid>/cgroup` shows where it is, and
`systemctl status crio-conmon-<pod ID>.scope` the scope.

An idle server of the static binaries has a resident set of about 3 MiB, most
of it the mapped pages of its binary (`RssFile` in `/proc/<pid>/status`, or
`RssShmem` for a binary on tmpfs), which the servers of all pods share, and
about 0.5 MiB of memory of its own (`Anonymous` in
`/proc/<pid>/smaps_rollup`, which is exact, while `RssAnon` can be off by
hundreds of kB on kernels since 6.2). Each container adds its buffers on top,
see [Architecture](architecture.md). Every spec of the integration tests
checks the resident set of the server (`VmRSS` in `/proc/<pid>/status`)
against the budget `MAX_RSS_KB` and its private anonymous memory against
`MAX_RSS_ANON_KB`, see the [Makefile](../Makefile). [Memory
profiling](#memory-profiling) shows where a server which grows spends it.

## Tracing

The server can export OpenTelemetry traces of its RPCs over OTLP gRPC. This
needs a build with the `tracing` feature:

```console
> cargo build --release --features tracing
```

The static release binaries and the RPMs get built without it, they accept
`--enable-tracing` and `--tracing-endpoint` and ignore them. Since unreleased,
such a server logs a warning at its start when it runs with `--enable-tracing`,
with a server log driver other than `none`, the default of CRI-O. The
dependency tree of a binary tells whether it has the feature:

```console
> conmonrs --version full | grep opentelemetry
├── opentelemetry v…
…
```

A build without the feature prints nothing here. Neither does one which got
built without the dependency tree, whose `cargo tree:` line of `--version full`
is empty, see [Build from source](install.md#build-from-source).

With the feature, `--enable-tracing` exports to `--tracing-endpoint`,
`http://127.0.0.1:4317` by default. The Go client starts the server with these
flags if its configuration has `Tracing: client.Tracing{Enabled: true}`. It then
passes the span of the context of a call along with the RPC, if there is one,
through the global propagator of OpenTelemetry (`otel.SetTextMapPropagator`),
which does nothing unless the caller registers one, like CRI-O does. The server
reads the W3C Trace Context, so the propagator has to write it, like
`propagation.TraceContext{}`. [contrib/tracing](../contrib/tracing) runs a local
Jaeger to look at the traces.

## Changes

- unreleased: the lines of the server log name the container they are about at
  every log level, through the span of its request or session, see
  [Server logs](#server-logs). Before, the spans only showed at the `debug`
  level, so a warning or an error did not tell which container it was about.
- unreleased: a journal which took whole chunks of container output again
  after a stall gets waited for again the next time it is busy, see
  [Journal stalls](#journal-stalls). Before, the lines it did not take right
  away got dropped without the wait until it took part of a chunk.
- unreleased: the server logs at `info` when a container exited, with its PID,
  its exit code and whether it got OOM killed, an OOM as it happens, a runtime or an
  exec process which got killed at its timeout and the containers it signals
  and kills at its shutdown, before only at `debug`. The end of a streaming
  session moved from `info` to `debug`, an exec or a session which failed from
  `error` to `warn`, output which got dropped a while after the exit from
  `debug` to `warn`.
- v1.2.2: the message of a panic goes to the journal with the server log
  drivers other than `stdout`, `none` included, before only to stderr. A
  server which did not fork writes it only to the stderr it kept.
- v1.2.2: `journalctl -t conmonrs` shows only the logs of the server. Before, it
  also showed the output of the containers which use the `journald` container
  log driver, whose entries got the identifier `conmonrs`.
- v1.2.2: once the lines of container output waited for the journal in vain,
  the following chunks drop what it does not take right away, before each of
  them waited up to about two seconds again.
- v1.2.1: the server logs carry the `SYSLOG_IDENTIFIER` `conmonrs`, before
  only their process name (`_COMM=conmonrs`).
