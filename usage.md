# Usage

To use conmon-rs with [CRI-O](https://github.com/cri-o/cri-o), please ensure
that you use at least:

- [CRI-O v1.25.2](https://github.com/cri-o/cri-o/releases/tag/v1.25.2)
- [conmon-rs v0.4.0](https://github.com/containers/conmon-rs/releases/tag/v0.4.0)

Alternatively, use their latest `main` versions which are mostly guaranteed to
work together.

## Configure CRI-O

CRI-O needs to be configured to use conmon-rs. To do this, change the runtime
configurations `runtime_type` and optionally the `monitor_path`, for example:

```console
> cat /etc/crio/crio.conf.d/99-runtimes.conf
```

```toml
[crio.runtime]
default_runtime = "runc"

[crio.runtime.runtimes.runc]
runtime_type = "pod"
monitor_path = "/path/to/conmonrs"  # Optional, lookup $PATH if not set
```

CRI-O should now use conmon-rs after a restart, which is being indicated by the
debug logs when creating a container:

```text
…
DEBU[…] Using conmonrs version: 0.4.0, tag: none, commit: 130bd1373835cdfef8ae066a87eb4becabbe440a, \
            build: 2022-11-09 10:36:18 +01:00, \
            target: x86_64-unknown-linux-gnu, \
            rustc 1.65.0 (897e37553 2022-11-02), \
            cargo 1.65.0 (4bc8f24d3 2022-10-20)  file="oci/runtime_pod.go:100"
…
```

## Configuring to use with Red Hat OpenShift

OpenShift 4.12 ships the latest version of conmon-rs per default. To use it,
just apply the following `MachineConfig` (for `runc`):

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
            source: data:,%5Bcrio.runtime.runtimes.runc%5D%0Aruntime_type%20%3D%20%22pod%22%0A
          mode: 420
          overwrite: true
          path: /etc/crio/crio.conf.d/99-conmonrs.conf
```

The same can be done for the `master` role or any other configured runtime like
`crun`.

### Using a custom conmonrs version

All conmonrs commits on `main` are build via [fedora
copr](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs).
This means that it's possible to install a custom version by running
`rpm-ostree`, for example for RHCOS 8:

```console
> rpm-ostree override replace https://download.copr.fedorainfracloud.org/results/rhcontainerbot/podman-next/epel-8-x86_64/05025896-conmon-rs/conmon-rs-0.0.git.1551.130d137-1.el8.x86_64.rpm
…
Upgraded:
  conmon-rs 0.4.0-2.rhaos4.12.git.el8 -> 101:0.0.git.1551.130bd137-1.el8
Use "rpm-ostree override reset" to undo overrides
Run "systemctl reboot" to start a reboot
> systemctl reboot
```
