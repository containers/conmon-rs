# Logging and OOM reporting

## Logging

Container output goes to the container log drivers requested per container,
unlike the logs of the server itself, which go to the server log driver
(`--log-driver`). When the next entry would exceed the configured maximum
size, the server writes the entries read before it to the old file and
replaces the log file with a new empty one, like conmon: it creates
`.<name>.conmonrs-tmp` next to it, a name which the log cleanup of the kubelet
does not match, and renames it over the path, so that a reader which follows
the log, like `kubectl logs -f`, reads the old file to its end first. The
rename unlinks the old file, the server keeps no rotated copy: its content is
gone once no reader holds it open anymore. Keeping rotated logs is up to the
caller, which renames the log file and then calls `reopenLogContainer`, as the
kubelet does through CRI-O: the server then starts a new file at the path the
same way. The server appends to the log files, so a file which gets truncated
in place, for example to free disk space, continues at its new end. A log file
which exists when the container gets created is appended to like with conmon,
its content counts for the maximum size. A write which fails in the
middle of an entry, like on a full disk, drops the rest of the entries read
with it, the next entry starts on a line of its own, and so does the first
entry after existing content without a final newline. A rotation which fails
drops the entries which would exceed the size, the next read of output tries
it again.

- `containerRuntimeInterface`: the CRI log format, one line per entry with an
  RFC 3339 timestamp in nanoseconds of when the server read the output, the
  stream, a `P` (partial) or `F` (full) marker and the line. The files it
  creates are readable by their group (mode `0640`) like with conmon.
- `journald`: one journal entry per line with the fields `CONTAINER_ID_FULL`,
  `CONTAINER_ID` (the first 12 characters) and `SYSLOG_IDENTIFIER`, at
  priority info for stdout and error for stderr, so that
  `journalctl CONTAINER_ID_FULL=<id>` or `CONTAINER_ID=<short id>` shows the
  output like with conmon. A line which had no ending yet carries
  `CONTAINER_PARTIAL_MESSAGE=true`. The maximum size does not apply. Like the
  option of conmon, `containerName` adds `CONTAINER_NAME`, which CRI-O passes.
  The `SYSLOG_IDENTIFIER` is the `containerName`, and the short ID without
  one, since v1.2.2.

The journald driver collects whole lines across reads, so a line
written in several pieces or a multi byte character split by a read still
ends up as one entry. Like the `P` marker of the CRI log, a partial entry is
a line which had no ending yet: it grew beyond 16 KiB, the output paused for
half a second, or the output ended without one.

Since v1.2.3, the server keeps a terminal open itself until the process
exited, so its output ends at the exit, not once no process has it open. A
process which closes the terminal and opens it again, like an init system or a
getty, keeps its output in the log and for attach clients. This takes Linux
4.13 or later (`TIOCGPTPEER`), on older kernels the output still ends once no
process has the terminal open.

The server does not sync the log files to disk on a rotation, the entries sit
in the page cache like other writes. Since v1.2.3, it syncs the CRI logs
once the container exited and its output ended, before it writes
the exit files, like conmon without `--no-sync-log`. CRI-O (since v1.31)
rejects its `no_sync_log` option for a `pod` runtime handler, so the sync
cannot be turned off. The exit files wait up to 5 seconds for the log drivers
to take the last output and for the sync, a slower disk does not hold them
back longer.

The server's own logs go to the server log driver, see [Server
logs](troubleshooting.md#server-logs). Running with `--log-level debug` prints
every RPC and I/O decision and is the first thing to try when debugging a
container that misbehaves under conmon-rs. CRI-O starts the servers with its
own log level and without logs, [Debugging a
pod](troubleshooting.md#debugging-a-pod) describes how to get them.

## OOM reporting

The server reports an OOM kill of a container through its `oomExitPaths`
(`OOMExitPaths` of the Go client): it creates each of them as an empty file
before it writes the exit files, so a client which sees an exit file can check
for the OOM file right away. The exit files wait for the OOM check, which
reads the counters of the cgroup once the process exited. Execs get no OOM
files.

With cgroup v2, the server watches `memory.events` of the cgroup of the
container process and takes a rise of its `oom_kill` counter for an OOM.
Kernels without that counter fall back to the `oom` counter, which also rises
when the OOM killer ends up killing nothing. crun places the process in a leaf
below the container cgroup with the same memory limit, the kernel counts the
kill in the leaf either way. As the systemd cgroup manager removes the leaf
once its processes exited, the server also watches the hierarchical counters
of the pod cgroup, the first one within two levels above the leaf with a name
like the kubelet gives them (`pod<uid>`, `kubepods-...-pod<uid>.slice`), and
without one those of the nearest ancestor which is no systemd scope. If the
leaf is gone at the exit, a kill in that cgroup which the leaf did not account
for, together with an exit by `SIGKILL` (status 137), counts as an OOM, which
may also be the one of another container in it. Without such a cgroup, or if
its counters cannot be read, an exit by `SIGKILL` with the leaf gone counts as
an OOM as well. A leaf without the memory controller, which has no
`memory.events`, does not count as gone: a kill counted by the cgroup above it
may be an OOM the same way, but without such a cgroup, or if its counters
cannot be read, nothing counts as one.

The server watches the memory events of all containers through a single
inotify instance, and reads the counters at most every 100 ms under memory
pressure. If it cannot watch them, it checks the counters only once the
process exited. It logs a warning when that happens for another reason than a
cgroup which is gone already, like a used up
`fs.inotify.max_user_instances` or `fs.inotify.max_user_watches`, once until
a watch works again.

Without cgroup v2 at `/sys/fs/cgroup`, like on a host with cgroup v1 or a
hybrid setup, the containers run, but the server reports no OOM kills (since
v1.2.3). Each server logs a warning about that once, with its first
container which has `oomExitPaths`. With the server log driver `none`, the
default of CRI-O, nothing shows it, see [Server
logs](troubleshooting.md#server-logs).

## Changes

- v1.2.3: the server reports OOM kills with cgroup v2 only. Before, it
  also watched the `memory.oom_control` of a cgroup v1 memory controller
  through an eventfd. On a host with cgroup v1 or a hybrid setup, the
  containers still run, and the server logs a warning once that it does not
  detect their OOM kills.
- v1.2.3: a `createContainer` request with the retired `json` log driver
  fails with
  `the json log driver of createContainer is not supported by this server`,
  one which sets the `tag` or `labels` of a log driver with
  `the tag of a log driver of createContainer is not supported by this server`
  or the same message for the `labels`, empty ones pass. Before, the `json`
  driver wrote one JSON object per line, and the `journald` driver wrote the
  `tag` as `CONTAINER_TAG` and `SYSLOG_IDENTIFIER` and the `labels` as fields
  of their own.
- v1.2.3: the server keeps a terminal open itself until the process
  exited. Before, the output ended once the terminal got closed the first
  time, and without stdin the server closed it as well.
- v1.2.3: the server syncs the CRI logs once the container exited
  and its output ended, before it never synced the container logs.
- v1.2.3: the server logs a warning when it cannot watch the memory events
  of a container for another reason than a cgroup which is gone already.
- v1.2.2: the OOM check watches the pod cgroup, or the nearest ancestor which
  is no systemd scope, above the leaf of crun. Before, it watched the parent of
  the leaf, the container scope which the systemd cgroup manager removes along
  with the leaf, so that any exit by `SIGKILL` with the leaf gone counted as an
  OOM.
- v1.2.2: a leaf without the memory controller does not count as gone for the
  OOM check.
- v1.2.2: the `SYSLOG_IDENTIFIER` of the `journald` container log driver is
  the `containerName` or the short ID, like for conmon. Before, it was
  `conmonrs`.
- v1.2.2: a log file which exists when the container gets created is appended
  to, before it got truncated.
- v1.2.2: the files of the `containerRuntimeInterface` driver are readable by
  their group (mode `0640`), before only by their owner.
