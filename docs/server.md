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

| Flag                              | Environment               | Default                 | Purpose                                                                              |
| --------------------------------- | ------------------------- | ----------------------- | ------------------------------------------------------------------------------------ |
| `-r, --runtime <RUNTIME>`         | `CONMON_RUNTIME`          |                         | Binary path of the OCI runtime, required for running the server.                     |
| `--runtime-dir <RUNTIME_DIR>`     | `CONMON_RUNTIME_DIR`      |                         | Directory for `conmon.sock`, the `pidfile` and the PID files of the execs, required. |
| `--runtime-root <RUNTIME_ROOT>`   | `CONMON_RUNTIME_ROOT`     |                         | Root directory passed to the OCI runtime (`--root`).                                 |
| `-l, --log-level <LEVEL>`         | `CONMON_LOG_LEVEL`        | `info`                  | `trace`, `debug`, `info`, `warn`, `error` or `off`.                                  |
| `-d, --log-driver <DRIVER>`       | `CONMON_LOG_DRIVER`       | `systemd`               | Where the server logs to: `systemd` (journald), `stdout` or `none`.                  |
| `--skip-fork`                     | `CONMON_SKIP_FORK`        | `false`                 | Stay in the foreground instead of forking into the background.                       |
| `-e, --enable-tracing`            | `CONMON_ENABLE_TRACING`   | `false`                 | Export OpenTelemetry traces (needs a build with the `tracing` feature).              |
| `-t, --tracing-endpoint <URL>`    | `CONMON_TRACING_ENDPOINT` | `http://127.0.0.1:4317` | OpenTelemetry gRPC endpoint.                                                         |
| `-v, --version [full]`            | `CONMON_VERSION`          |                         | Print the version, `full` includes the dependency tree.                              |
| `-j, --version-json, --vj [full]` | `CONMON_VERSION_JSON`     |                         | Print the version as JSON.                                                           |

`--skip-fork` and `--enable-tracing` take no value. Their environment variables
take a boolean: `1`, `true`, `yes`, `y`, `t` or `on` for true and `0`, `false`,
`no`, `n`, `f` or `off` for false, in any case.

Before it forks, the server sets its umask to 022 and removes `NOTIFY_SOCKET`
from its environment, so that the runtime does not get the sd_notify socket of
the caller. It lowers its own OOM score to -1000, turns transparent huge pages
off for itself and raises its soft limit of open files to the hard one. The OCI
runtime gets the OOM score, the huge pages setting and the open files limits the
server started with, so the container processes get the OOM score of the client
unless it sets `process.oomScoreAdj` in the OCI configuration, like CRI-O does.

When the server forks, the parent writes the child's PID to `pidfile` in the
runtime directory and exits. With a server log driver other than `stdout`, the
child replaces its stdin, stdout and stderr with `/dev/null` like a daemon, so
neither the server nor the processes it starts write to the ones of the caller
anymore. The server closes the other file descriptors it inherited, changes into
`/`, acts as child subreaper for the containers and listens on `conmon.sock` in
the runtime directory. `conmon.sock` gets mode `0700` on bind, like the attach
sockets.

The server adds arguments of its own to every runtime call for a container or
an exec, before the arguments of the request: `--root=<RUNTIME_ROOT>`
for `--runtime-root`, and `--systemd-cgroup` for the `systemd` cgroup manager
of the request. If the file name of the runtime is `crun`, it also adds
`--log=journald:<container ID>`, and for the log levels `trace` and `debug`
of the server `--log-level=debug`, for `warn` `--log-level=warning` and for
`error` `--log-level=error`. crun keeps its default for `info` and `off`.

On SIGTERM or SIGINT, the server kills the containers and removes `conmon.sock`
within 9.5 seconds, before `Shutdown` of the Go client, which sends SIGINT,
gives up after 10 with `ErrShutdownTimeout`. If any of that fails or does not
finish in time, the server logs it as an error and exits with status 1.
A server which cannot listen for these signals does the same cleanup right away
and exits with status 1.

On SIGTERM, the server (since unreleased) first forwards the SIGTERM to the
containers, like conmon, and gives them up to 5 of the 9.5 seconds to exit on
it with their own exit codes before it kills them. Another SIGTERM or a SIGINT
meanwhile kills them right away. Exec processes and containers created
meanwhile do not get the SIGTERM. A container which ignores it, like the init
of a PID namespace without a handler for it, delays the shutdown by the full 5
seconds.

Runtime calls in progress, like the `create` of a container or an exec, get up
to 5 seconds after the containers got killed to finish. Since unreleased, the
server stops the ones which did not finish by then, or 7 of the 9.5 seconds
after the signal at the latest, so also after the 5 seconds of SIGTERM: it
kills their runtimes and the processes they started, the calls fail, and the
server exits with status 1.

## Changes

- unreleased: on SIGTERM, the server forwards the SIGTERM to the containers
  and gives them up to 5 seconds to exit on it, before it killed them right
  away.
- v1.2.2: the server passes `--log-level=debug` to crun for its log level
  `trace` as well, before crun kept its default there.
- v1.2.2: `conmon.sock` gets mode `0700` on bind, before `0755` from the umask
  `022`, which the server sets itself. Connecting needs write access, so only
  its owner and root could connect before as well.
