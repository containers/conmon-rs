# CRI-O

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
runtime_path = "/usr/bin/crun"
runtime_type = "pod"
runtime_root = "/run/crun"
monitor_path = "/path/to/conmonrs"  # Optional, lookup $PATH if not set
```

A table of a runtime handler in a drop-in file replaces the whole handler of
that name rather than merging its keys into the one of `crio.conf` or of an
earlier drop-in. The table of the example is the complete definition of the
`crun` handler, so it has to list every other option the handler needs, like
its `allowed_annotations` or `monitor_env`, as well.

The `pod` runtime type is what selects conmon-rs. A `monitor_path` pointing to
conmonrs with the default `oci` runtime type does not work, CRI-O expects conmon
there and rejects the configuration.

CRI-O should now use conmon-rs after a restart. Since v1.34, CRI-O logs the
version of the binary of each runtime handler when it starts, since v1.38 and
in the releases of the 1.37 branch after v1.37.2 with the
[features](protocols.md#features) as well:

```text
INFO[…] Runtime handler "crun" is using conmon-rs version: …, tag: …, \
            commit: …, build: …, target: x86_64-unknown-linux-gnu, rustc …, cargo …, \
            features: …
```

Older ones only show it in the debug logs when they create a container:

```text
…
DEBU[…] Using conmonrs version: …, tag: …, commit: …, build: …, \
            target: x86_64-unknown-linux-gnu, rustc …, cargo …  file="oci/runtime_pod.go"
…
```

Whether CRI-O serves exec, attach and port forwarding itself or hands them to
the [streaming server](protocols.md#streaming-server) of conmon-rs depends on
`stream_websockets` of the runtime handler, the sections about the streaming
server only apply to a handler which has it on:

- Up to v1.37, CRI-O serves them itself by default. Only with
  `stream_websockets = true`, since v1.34, it hands exec and attach to
  conmon-rs.
- v1.38 and `main` turn `stream_websockets` on for a `pod` runtime handler which
  does not set it and whose binary reports the `streamingprotocols`
  [feature](protocols.md#features), and log
  `Enabling streaming over websockets for runtime handler "…" by default`. They
  leave it off if `stream_enable_tls` or `infra_ctr_cpuset` of CRI-O or
  `exec_cpu_affinity` of the handler is set: the streaming server speaks plain
  HTTP, and the server neither runs the exec sessions it serves in the exec
  cgroup of `exec_cpu_affinity` nor pins them to the `infra_ctr_cpuset`. An
  explicit `stream_websockets = true` stays on with these options, CRI-O only
  warns about them, and `stream_websockets = false` keeps the streaming of
  CRI-O.

CRI-O v1.38, `main` and the releases of the 1.37 branch after v1.37.2 hand port
forwarding to a binary with the `portforward` feature as well, and turn a
`stream_websockets = true` off with a warning for a binary without the
`streamingprotocols` feature. v1.34 to v1.37.2 only do that for a binary before
v0.7.0, so leave it off there with a server before v1.2.0: those servers only
speak `v5.channel.k8s.io`, while the kubelet talks SPDY to them. The
`stream_address`, `stream_port`, `stream_enable_tls` and `stream_tls_*` options
of CRI-O do not apply to the sessions of conmon-rs: its streaming server listens
on `127.0.0.1` with a random port in plain HTTP, see [Streaming
security](protocols.md#streaming-security). Of the streaming options, only
`stream_idle_timeout` reaches the server, as the idle timeout of the sessions in
the CRI-O versions which pass one, see [Exec and
attach](protocols.md#exec-and-attach).

CRI-O versions since v1.25 support conmon-rs, the `main` branches of both
projects are kept working together. Which server versions a CRI-O release
works with depends on the client it vendors, see
[Compatibility](#compatibility).

The `monitor_env` of the runtime handler configures the servers which CRI-O
starts for it:

| Entry                          | Effect                                                                                                                                                                                                                                | Since CRI-O |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- |
| `LOG_DRIVER=<driver>`          | The server log driver (`--log-driver`): `systemd`, `stdout` or `none`. Without it, CRI-O starts the servers with `none`, see [Server logs](troubleshooting.md#server-logs) and [Debugging a pod](troubleshooting.md#debugging-a-pod). | v1.34       |
| `HEAPTRACK_OUTPUT_PATH=<path>` | Profiles the memory of the servers with heaptrack, see [Memory profiling](troubleshooting.md#memory-profiling).                                                                                                                       | v1.34       |
| `HEAPTRACK_BINARY_PATH=<path>` | The heaptrack binary, also enables the profiling.                                                                                                                                                                                     | v1.34       |
| Any other `KEY=value`          | Part of the environment of the server, which passes it on to the runtime and its hooks, together with `CRI_O_EXTERNAL=1` and the `PATH` and `XDG_RUNTIME_DIR` of CRI-O. `CONMON_*` variables configure the server like its flags.     | v1.38       |

CRI-O v1.37.2 and older start the servers with the environment of CRI-O and
use no other `monitor_env` entries for them. The releases of the 1.37 branch
after v1.37.2 pass them on as well.

CRI-O always passes `--runtime`, `--runtime-dir`, `--runtime-root`,
`--log-level` and `--log-driver`, which take precedence over their `CONMON_*`
variables. Only the variables of the other flags take effect, in practice the
tracing ones while the tracing of CRI-O is off, and only with a build with the
`tracing` feature, see [Tracing](troubleshooting.md#tracing). Do not set
`CONMON_SKIP_FORK`, `CONMON_VERSION` or `CONMON_VERSION_JSON`: the clients up to
v1.2.1, which the CRI-O releases up to v1.37 vendor, pass them on to the server,
which then hangs or fails every pod. The client of v1.2.2, which CRI-O v1.38 and
`main` vendor, drops them.

Some CRI-O options do not reach the server, or only in newer versions. Up to
v1.37, CRI-O passes `no_pivot` only to conmon, the server creates the
containers without `--no-pivot`. v1.38 and `main` pass `--no-pivot` to the
server as well, in the `CommandArgs` of the create. CRI-O applies the
`container_create_timeout` of a runtime handler only to conmon and VM
runtimes: with a `pod` runtime handler, it waits for the server as long as the
CRI request lasts, and a server since v1.2.0 kills a runtime which takes longer
than its own limit of 10 minutes to create a container, see [Cap'n Proto
RPC](protocols.md#capn-proto-rpc). A create which CRI-O gives up on still
finishes on the server, which keeps watching the container. The
`platform_runtime_paths` of a runtime handler only apply to some of the runtime
calls which CRI-O makes itself, like update and checkpoint: the server runs the
`runtime_path` of the handler for the containers it creates and the commands
it executes in them. Restoring a checkpoint with a `pod` runtime handler needs
a server with the `restore` [feature](protocols.md#features) (since unreleased)
and a CRI-O which restores through the client, with the `RestorePath` of
`CreateContainerConfig`: CRI-O so far, including `main`, runs the
`monitor_path` binary with the arguments of conmon for it, which conmon-rs
rejects, so the restore fails before the runtime starts. Checkpointing works
with every version, CRI-O runs the runtime for it itself. Like for a create,
the server runs the `runtime_path` of the handler for the restore, with the
options CRI-O passes, like `--no-pivot` and the SELinux options of runc. A
CRI-O which restores through the client takes the PID of the restored init
process from the response of `CreateContainer`, like after a create: the
server writes no conmon PID file into the bundle, which the conmon path of
CRI-O reads after a restore with `ReadConmonPidFile`. The checkpoint and the
restore need criu and a runtime built with CRIU support on the node.

## Compatibility

A CRI-O release vendors one version of the Go client, which needs a server
it can start and talk to. A server of v1.0.0 or newer does not work with the
clients before v1.0.0, and the other way around:

| CRI-O         | Vendored client                                            | Servers which work |
| ------------- | ---------------------------------------------------------- | ------------------ |
| v1.25, v1.26  | v0.4.0, a commit before v0.2.0 in v1.25.0 and v1.25.1      | v0.4.0 to v0.8.0   |
| v1.27, v1.28  | v0.5.0, v0.5.1                                             | v0.5.0 to v0.8.0   |
| v1.29, v1.30  | a commit before v0.6.2                                     | v0.6.2 to v0.8.0   |
| v1.31         | v0.6.5                                                     | v0.6.5 to v0.8.0   |
| v1.32, v1.33  | v0.6.6                                                     | v0.6.6 to v0.8.0   |
| v1.34         | v0.7.2                                                     | v0.7.2 to v0.8.0   |
| v1.35         | a commit before v0.7.3                                     | v0.7.3, v0.8.0     |
| v1.36         | v0.7.3                                                     | v0.7.3, v0.8.0     |
| v1.37         | v1.0.1 up to v1.37.2, v1.2.1 in the releases after v1.37.2 | v1.0.0 and newer   |
| v1.38, `main` | v1.2.2                                                     | v1.0.0 and newer   |

The clients before v1.0.0 start the server with `--cgroup-manager`, which
v1.0.0 removed: a newer server exits right away with
`error: unexpected argument '--cgroup-manager' found`, and CRI-O fails to
create the pod. Since v0.6.0, the server takes the cgroup manager from each
request instead, which the clients before v0.6.0 do not send, so CRI-O v1.25
to v1.28 with the `cgroupfs` cgroup manager need a server before v0.6.0.

v1.0.0 also broke the wire format: changing the type of the `metadataOld`
fields from `Data` to `Void` moved the pointer slots of the fields after them,
for example `metadata` of `CreateContainerRequest` from pointer 9 to 8. Across
the break, both sides read these fields from the wrong slots. CRI-O sets none of
them besides the tracing metadata, so its requests only lose the tracing
metadata. A client of v1.1.0 or newer reads the
[features](protocols.md#features) of a server before v1.0.0 as empty.

From v1.0.0 on, a newer client works with an older server: it checks the
features of the server before it relies on them, and an older server ignores the
fields it does not know, the [features](protocols.md#features) tell which of
them a server uses.

## Upgrading

Servers keep running when their binary gets replaced or CRI-O restarts. Each
pod keeps its server, and with it its features, until the pod gets removed,
only new pods get a server of the new binary. A restarted CRI-O connects to
the servers of the existing pods again, which works as long as both are of
v1.0.0 or newer, see [Compatibility](#compatibility).

Upgrade and downgrade the `conmonrs` binary together with CRI-O whenever the
client of the new CRI-O needs another server version. Between CRI-O v1.36 and
v1.37 this crosses the wire format break of v1.0.0: CRI-O v1.36 and older
cannot start a server of v1.0.0 or newer, and the old servers of the pods
which keep running report no features to a newer CRI-O. They still serve its
requests, which carry no affected field besides the tracing metadata, but a
CRI-O with the client of v1.2.1 or newer serves exec, attach and port
forwarding for these pods with its own streaming server instead. Drain the node
before an upgrade or downgrade across the break, or recreate its pods after it,
to give them servers of the new version.

An upgrade to CRI-O v1.38 turns `stream_websockets` on for the `pod` runtime
handlers which do not set it, unless the binary or one of the options named in
[Configure CRI-O](#configure-cri-o) keeps it off. Set
`stream_websockets = false` to keep the streaming of CRI-O.

## Red Hat OpenShift

OpenShift ships conmon-rs. To use it for a runtime, apply a `MachineConfig`
with a drop-in which sets the runtime type, for example for `crun`. The
drop-in replaces the whole handler, see [Configure CRI-O](#configure-cri-o),
so it repeats the other options of the handler which OpenShift defines. The
ones below are those of the `crun` handler in the `crio.yaml` templates of the
machine-config-operator; compare them with the handler of your release, for
example with `crio config` in an `oc debug node` shell. The file holds:

```toml
[crio.runtime.runtimes.crun]
runtime_root = "/run/crun"
runtime_type = "pod"
allowed_annotations = [
    "io.containers.trace-syscall",
    "io.kubernetes.cri-o.Devices",
    "io.kubernetes.cri-o.LinkLogs",
]
container_min_memory = "512KiB"
default_annotations = {"run.oci.systemd.subgroup" = ""}
```

As a `MachineConfig`:

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
            source: data:,%5Bcrio.runtime.runtimes.crun%5D%0Aruntime_root%20%3D%20%22%2Frun%2Fcrun%22%0Aruntime_type%20%3D%20%22pod%22%0Aallowed_annotations%20%3D%20%5B%0A%20%20%20%20%22io.containers.trace-syscall%22%2C%0A%20%20%20%20%22io.kubernetes.cri-o.Devices%22%2C%0A%20%20%20%20%22io.kubernetes.cri-o.LinkLogs%22%2C%0A%5D%0Acontainer_min_memory%20%3D%20%22512KiB%22%0Adefault_annotations%20%3D%20%7B%22run.oci.systemd.subgroup%22%20%3D%20%22%22%7D%0A
          mode: 420
          overwrite: true
          path: /etc/crio/crio.conf.d/99-conmonrs.conf
```

The same can be done for the `master` role or any other configured runtime.
Builds of every `main` commit are available in the [podman-next
copr](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs)
and can be installed on RHCOS with `rpm-ostree override replace <rpm url>`.
They are of v1.0.0 or newer, which a CRI-O of v1.36 or older cannot start,
see [Compatibility](#compatibility). The override takes effect with the next
boot, which gives all pods servers of the new version.
