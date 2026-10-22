<p align="center"><img  height="200" src="./.github/logo/conmon-rs.png"></p>

[![ci](https://github.com/containers/conmon-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/containers/conmon-rs/actions/workflows/ci.yml)
[![gh-pages](https://github.com/containers/conmon-rs/actions/workflows/gh-pages.yml/badge.svg)](https://github.com/containers/conmon-rs/actions/workflows/gh-pages.yml)
[![codecov](https://codecov.io/gh/containers/conmon-rs/branch/main/graph/badge.svg)](https://codecov.io/gh/containers/conmon-rs)
[![dependencies](https://deps.rs/repo/github/containers/conmon-rs/status.svg)](https://deps.rs/repo/github/containers/conmon-rs)
[![builds](https://img.shields.io/badge/packages-copr-orange.svg)](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs)
[![docs](https://img.shields.io/badge/docs-main-blue.svg)](https://containers.github.io/conmon-rs/conmonrs/index.html)
[![go-docs](https://pkg.go.dev/badge/github.com/containers/conmon-rs/pkg/client.svg)](https://pkg.go.dev/github.com/containers/conmon-rs/pkg/client)

A pod level OCI container runtime monitor for Linux.

conmon-rs does what [conmon](https://github.com/containers/conmon) does for a
single container, for a whole pod: a container engine like
[CRI-O](https://github.com/cri-o/cri-o) starts one `conmonrs` process per pod,
and that process creates the containers of the pod through the OCI runtime,
holds their standard streams open, forwards their output to log files or
journald, serves attach and exec sessions, detects out of memory kills and
writes the exit codes once the containers terminate. The engine talks to it over
a UNIX domain socket, so no new monitor process is needed for additional
containers or execs. It works with cgroup v1 and v2 and any OCI runtime with
the `create`, `exec` and console socket conventions of runc, like crun.

## Documentation

- [usage.md](usage.md): configuring CRI-O, the command line, the `pause`
  subcommand and the protocols between the server and its clients.
- [release.md](release.md): how to cut a release.
- [Rust API docs](https://containers.github.io/conmon-rs/conmonrs/index.html)
  and [Go client docs](https://pkg.go.dev/github.com/containers/conmon-rs/pkg/client).

## Obtain the latest version

We provide statically linked binaries for every successfully built commit on
`main` via our [Google Cloud Storage Bucket][bucket]. Our provided [get
script](scripts/get) can be used to download the latest version:

```console
> curl https://raw.githubusercontent.com/containers/conmon-rs/main/scripts/get | bash
```

It is also possible to select a specific git SHA or the output binary path by:

```console
> curl https://raw.githubusercontent.com/containers/conmon-rs/main/scripts/get | \
    bash -s -- -t $GIT_SHA -o $OUTPUT_PATH
```

The script automatically verifies the created sigstore signatures if the local
system has [`cosign`](https://github.com/sigstore/cosign) available in its
`$PATH`.

The binaries also come with [SLSA](https://slsa.dev) Build L3 provenance and
an SPDX 2.3 SBOM, both as GitHub artifact attestations. A job without access
to the signing credentials builds them, the reusable [publish-static
workflow](.github/workflows/publish-static.yml), which runs no build steps,
attests and signs them. Verify them with the [GitHub
CLI](https://cli.github.com):

```console
> gh attestation verify conmonrs.amd64 \
    --repo containers/conmon-rs \
    --signer-workflow containers/conmon-rs/.github/workflows/publish-static.yml \
    --source-ref refs/heads/main
```

Add `--predicate-type https://spdx.dev/Document/v2.3` for the SBOM. The
bucket and the releases carry the attestation bundles as well
(`conmonrs.$ARCH.intoto.jsonl` and `conmonrs.$ARCH.spdx.intoto.jsonl`), and
the SBOM itself (`conmonrs.$ARCH.spdx.json`). The cosign signatures
(`conmonrs.$ARCH.bundle`) have the identity
`https://github.com/containers/conmon-rs/.github/workflows/publish-static.yml@refs/heads/main`,
the one of commits from before the attestations is
`https://github.com/containers/conmon-rs/.github/workflows/ci.yml@refs/heads/main`.

The source tarball and the `sha256sums.txt` of a release have SLSA Build L3
provenance and cosign signatures of the reusable [build-sources
workflow](.github/workflows/build-sources.yml) for the tag:

```console
> gh attestation verify conmonrs-vX.Y.Z.tar.gz \
    --repo containers/conmon-rs \
    --signer-workflow containers/conmon-rs/.github/workflows/build-sources.yml \
    --source-ref refs/tags/vX.Y.Z
> cosign verify-blob sha256sums.txt \
    --bundle sha256sums.txt.bundle \
    --certificate-identity https://github.com/containers/conmon-rs/.github/workflows/build-sources.yml@refs/tags/vX.Y.Z \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Every commit on `main` is also built as an RPM in the [podman-next
copr](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs).

[bucket]: https://console.cloud.google.com/storage/browser/cri-o/conmon-rs

## Architecture

The repository contains two components:

1. The Rust server in [conmon-rs/server](./conmon-rs/server), the `conmonrs`
   binary. Its Cap'n Proto interface lives in
   [conmon-rs/server/proto/conmon.capnp](./conmon-rs/server/proto/conmon.capnp).
2. The Go client in [pkg/client](./pkg/client), which starts the server, talks
   to it over Cap'n Proto RPC and hides the RPC types behind plain Go
   structures. CRI-O vendors this client.

The server is started once per pod by the client, forks into the background and
listens on a UNIX domain socket for RPCs. The client runs the following
operations against it:

- **Create container**: the server runs the OCI runtime's `create`, keeps the
  container's standard streams (pipes or a pseudo terminal received over the
  runtime's console socket), writes the output to the configured log drivers
  (CRI format, JSON or journald) and watches the container's cgroup for out of
  memory events. When the container exits, the exit code is written to the exit
  paths and an optional cleanup command is run. With `sdnotifySocket` set, the
  server also relays the sd_notify messages of the container from
  `<bundle>/notify/notify.sock` to that host socket, like conmon does with
  `--sdnotify-socket`.
- **Exec**: `execSyncContainer` runs a command inside a container and returns
  its output and exit code. For interactive execs and attaches
  (`serveExecContainer`, `serveAttachContainer`), the server opens a streaming
  server on localhost which speaks the Kubernetes remote command protocols over
  WebSocket and SPDY, so CRI-O can hand the URL straight to the kubelet. The
  same server forwards ports into the pod network namespace over SPDY or
  WebSocket (`servePortForwardContainer`).
- **Attach**: `attachContainer` serves output and stdin of a container on a per
  container attach socket, the protocol conmon speaks as well.
- **Namespaces**: `createNamespaces` creates the pod level namespaces and bind
  mounts them, using the `conmonrs pause` subcommand to hold a PID namespace
  open. This replaces what CRI-O otherwise does with `pinns`.
- **File descriptors**: `startFdSocket` opens a socket on which a client passes
  file descriptors to the server, for example to leak into or hand over to a
  container.
- **Maintenance**: `reopenLogContainer` rotates the log files of a container,
  `setWindowSizeContainer` resizes its terminal, `version` reports the server
  build.

The server is single threaded: a tokio `current_thread` runtime drives all I/O,
the Cap'n Proto RPC runs on one additional thread, and blocking work like file
I/O goes to a small bounded pool. All containers of a pod share one inotify
instance for OOM detection. Per container, the server keeps two 4 KiB read
buffers, the channels for exec output and a few tasks, which is why a pod with
many containers still fits in a few megabytes.

The following diagram shows how the pieces connect:

```mermaid
flowchart TB
    subgraph engine["Container engine, for example CRI-O"]
        client["Go client<br/>pkg/client"]
        kubelet["kubelet"]
    end

    subgraph server["conmonrs, one process per pod"]
        rpc["Cap'n Proto RPC<br/>conmon.sock"]
        reaper["Child reaper"]
        subgraph percontainer["Per container"]
            io["Container I/O<br/>terminal or pipes"]
            logs["Log drivers<br/>CRI, JSON, journald"]
            attachsock["Attach socket"]
            oom["OOM watcher"]
            exitpaths["Exit paths and<br/>cleanup command"]
        end
        inotify["Shared inotify instance"]
        streaming["Streaming server<br/>WebSocket and SPDY on localhost"]
        fdsock["fd socket"]
        pause["conmonrs pause<br/>pod namespaces"]
    end

    runtime["OCI runtime<br/>crun or runc"]

    client -- "CLI start, PID file" --> server
    client -- "createContainer, execSync, attach,<br/>reopenLog, setWindowSize" --> rpc
    rpc --> reaper
    reaper <-- "create and exec over the CLI,<br/>container PID and console socket back" --> runtime
    reaper --> io
    io --> logs
    io --> attachsock
    reaper --> oom
    inotify --> oom
    reaper --> exitpaths
    rpc -- "serveExec, serveAttach" --> streaming
    kubelet -- "v5.channel.k8s.io" --> streaming
    rpc -- "startFdSocket" --> fdsock
    client -- "SCM_RIGHTS" --> fdsock
    rpc -- "createNamespaces" --> pause
```

## Development

The [Makefile](./Makefile) has targets for everything CI runs: `make` builds the
server, `make lint` runs rustfmt, clippy, golangci-lint and shellcheck (which has
to be installed), `make unit` runs the Rust unit tests and `make integration`
runs the Go integration suite against the release binary (`RUNTIME_PATH` selects the OCI runtime, the suite needs root for
the namespace and OOM specs). `make help` lists the rest. Contributions follow
[CONTRIBUTING.md](./CONTRIBUTING.md).
