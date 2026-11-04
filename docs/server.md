# Server

conmon-rs is started by a container engine, not by hand. The engine runs the
`conmonrs` binary once per pod, the binary forks into the background and the
engine talks to it over a UNIX domain socket. The Go client in
[pkg/client](../pkg/client) does this for
[CRI-O](https://github.com/cri-o/cri-o) and is the reference for other
clients.

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
runtime directory and exits. With a server log driver other than `stdout`, the
child replaces its stdin, stdout and stderr with `/dev/null` like a daemon, so
neither the server nor the processes it starts write to the ones of the caller
anymore. The server closes the other file descriptors it inherited, changes into
`/`, acts as child subreaper for the containers and listens on `conmon.sock` in
the runtime directory. Since v1.2.2, `conmon.sock` and the [fd
socket](protocols.md#file-descriptor-socket) get mode `0700` on bind like the
attach sockets, instead of `0755` from the umask `022`, which the server sets
itself. Connecting needs write access, so only their owner and root could
connect before as well.

The server adds arguments of its own to every runtime call for a container or
an exec, before the arguments of the request: `--root=<RUNTIME_ROOT>`
for `--runtime-root`, and `--systemd-cgroup` for the `systemd` cgroup manager
of the request. If the file name of the runtime is `crun`, it also adds
`--log=journald:<container ID>`, and for the log levels `trace` and `debug`
of the server `--log-level=debug`, for `warn` `--log-level=warning` and for
`error` `--log-level=error`. crun keeps its default for `info` and `off`, and
before v1.2.2 for `trace` as well.

On SIGTERM or SIGINT, the server kills the containers, stops the pause process
and removes its sockets within 9.5 seconds, before `Shutdown` of the Go client,
which sends SIGINT, gives up after 10 with `ErrShutdownTimeout`. If any of that
fails or does not finish in time, the server logs it as an error and exits with
status 1. A server which cannot listen for these signals does the same cleanup
right away and exits with status 1. Since unreleased, the server first forwards
a SIGTERM to the containers, like conmon, and gives them up to 5 of the 9.5
seconds to exit on it with their own exit codes before it kills them. Another
SIGTERM or a SIGINT meanwhile kills them right away. Exec processes and
containers created meanwhile do not get the SIGTERM. A container which ignores
it, like the init of a PID namespace without a handler for it, delays the
shutdown by the full 5 seconds.

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
removed when the server shuts down. Since v1.2.2, the bind mounts get
unmounted lazily (`MNT_DETACH`), a process which still holds one open keeps its
namespace alive until it lets go of it. Before, a bind path which a process
held open stayed mounted and its file stayed behind.
