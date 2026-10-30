# Contributing to conmon-rs

We'd love to have you join the community! The following summarizes the
processes that we follow.

## Topics

- [Reporting Issues](#reporting-issues)
- [Submitting Pull Requests](#submitting-pull-requests)
- [Communications](#communications)
- [Security](#security)
- [Code of Conduct](#code-of-conduct)

## Reporting Issues

Before reporting an issue, check our backlog of
[open issues](https://github.com/containers/conmon-rs/issues)
to see if someone else has already reported it. If so, feel free to add
your scenario, or additional information, to the discussion. Or simply
"subscribe" to it to be notified when it is updated.

If you find a new issue with the project we'd love to hear about it! The most
important aspect of a bug report is that it includes enough information for
us to reproduce it. So, please include as much detail as possible and try
to remove the extra stuff that doesn't really relate to the issue itself.
The easier it is for us to reproduce it, the faster it'll be fixed!

Please don't include any private/sensitive information in your issue!
Security issues get reported privately, see the [Security](#security) section.

## Submitting Pull Requests

No Pull Request (PR) is too small! Typos, additional comments in the code,
new test cases, bug fixes, new features, more documentation, ... it's all
welcome!

While bug fixes can first be identified via an "issue", that is not required.
It's ok to just open up a PR with the fix, but make sure you include the same
information you would have included in an issue, like how to reproduce it.

PRs for new features should include some background on what use cases the
new code is trying to address. When possible and when it makes sense, try to
break up larger PRs into smaller ones, smaller code changes are easier to
review. But only if those smaller ones make sense as stand-alone PRs.

Regardless of the type of PR, all PRs should include:

- well documented code changes
- additional test cases. Ideally, they should fail without your code change
  applied
- documentation changes

Squash your commits into logical pieces of work that might want to be reviewed
separate from the rest of the PRs. But, squashing down to just one commit is ok
too since in the end the entire PR will be reviewed anyway. When in doubt,
squash.

Format your changes with `make fmt`, then check them with the linters and the
unit tests of the server and the Go client:

```console
$ make lint unit unit-go
```

Changes to Markdown, YAML or JSON files have to pass prettier, which `make
prettier` applies and `make verify-prettier` checks, both in a container. `make
verify` runs the other checks of CI as well.

PRs which change the Cap'n Proto schema in
[conmon-rs/server/proto/conmon.capnp](conmon-rs/server/proto/conmon.capnp) run
`make update-proto` and commit the regenerated Go code in
[internal/proto](internal/proto), CI checks that it matches. The target runs
the `capnp` of the nixpkgs in flake.lock through nix, `make update-proto
CAPNP=capnp` uses a local one instead.

Servers keep running across upgrades, so clients and servers of different
releases talk to each other, see the compatibility section of
[usage.md](usage.md#compatibility). The schema has to stay compatible on the
wire:

- Only add fields, methods and enumerants, each with the next free ordinal.
- Never change the type or the ordinal of a field, and never remove one.
  v1.0.0 changed the type of the `metadataOld` fields from `Data` to `Void`,
  which moved the pointer slots of the fields after them and broke every
  client and server of another major version.
- Retire a field by renaming it, for example to `fooOld`, with its type
  unchanged, and document that the server ignores it.
- New behavior which a client has to know about before it relies on it gets
  a feature string in
  [version.rs](conmon-rs/server/src/version.rs) and a matching `Feature`
  constant in [pkg/client](pkg/client/version.go), see the features section
  of usage.md. An older server ignores new fields, so the client checks the
  feature first.
- Mark new limits and fields with "since unreleased" in the schema, the Go
  documentation and usage.md, the release replaces it with the version.

`make verify-schema-layout`, which CI runs, guards these rules: it fails if a
change moves an existing field of the schema to another slot.

The unit tests run as a regular user, and so do most integration tests. The
OOM and namespace specs need elevated permissions for the cgroups and bind
mounts and skip or fail otherwise. Like CI, build the server, download the Go
modules and install ginkgo as your user first, so that `target` and `.build`
do not end up owned by root. Then run the suite as root with the `PATH` which
finds `go`, your Go settings like `GOPROXY` (`GOENV`), your module cache and
your Go toolchain. Leave out `GOCACHE`, root would leave files of its own in
your build cache:

```console
$ make unit unit-go
$ make release .install.ginkgo
$ go mod download && go -C pkg/client/integration mod download
$ sudo env PATH="$PATH" GOENV="$(go env GOENV)" GOMODCACHE="$(go env GOMODCACHE)" \
    GOTOOLCHAIN="$(go env GOTOOLCHAIN)" make integration CONMON_BINARY=target/release/conmonrs
```

`make unit` runs the Rust unit tests with and without the `tracing` feature,
`make unit-go` the unit tests of the Go client, which need no server.

`CONMON_BINARY` selects the server to test, which the target builds
otherwise, for example the static build of `make release-static`:

```console
$ sudo env PATH="$PATH" GOENV="$(go env GOENV)" GOMODCACHE="$(go env GOMODCACHE)" \
    GOTOOLCHAIN="$(go env GOTOOLCHAIN)" make integration CONMON_BINARY=result/bin/conmonrs MAX_RSS_KB=5500
```

`make release-static` builds the static binary for the local architecture,
the flake supports x86_64 and aarch64 Linux hosts. `nix build .#amd64`,
`.#arm64`, `.#ppc64le` or `.#s390x` builds the binary of an architecture into
`result/bin/conmonrs`, cross compiled on the other ones, like CI does.

PRs that fix issues should include a reference like `Fixes #XXXX` in the PR
description, as the PR template asks, so that GitHub closes the referenced
issue when the PR is merged.

Most PRs will be reviewed by the [OWNERS](OWNERS).

### Sign your PRs

The sign-off is a line at the end of the explanation for the patch. Your
signature certifies that you wrote the patch or otherwise have the right to pass
it on as an open-source patch. The rules are simple: if you can certify
the below (from [developercertificate.org](https://developercertificate.org/)):

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.
660 York Street, Suite 102,
San Francisco, CA 94110 USA

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.

Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

Then you just add a line to every git commit message:

    Signed-off-by: John Doe <john.doe@email.com>

Use your real name (sorry, no pseudonyms or anonymous contributions.)

If you set your `user.name` and `user.email` git configs, you can sign your
commit automatically with `git commit -s`.

## Communications

For general questions, or discussions, please use our [channel on the Kubernetes slack](https://kubernetes.slack.com/archives/crio).

For discussions around issues/bugs and features, you can use the GitHub
[issues](https://github.com/containers/conmon-rs/issues) and
[PRs](https://github.com/containers/conmon-rs/pulls) tracking system.

## Security

Report security issues privately as described in the
[security policy](SECURITY.md).

## Code of Conduct

This project follows the
[containers Code of Conduct](https://github.com/containers/common/blob/main/CODE-OF-CONDUCT.md).
