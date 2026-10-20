# Creating a new release

Creating a new conmon-rs release can be done by proposing a PR to bump the
`version` in the `[workspace.package]` section of [Cargo.toml](Cargo.toml)
and the `Version` in [rpm/conmon-rs.spec](rpm/conmon-rs.spec), and running
`cargo update --workspace` to update [Cargo.lock](Cargo.lock).

If the Cap'n Proto schema in
[conmon-rs/server/proto/conmon.capnp](conmon-rs/server/proto/conmon.capnp)
changed since the last release, run `make update-proto` and commit the
regenerated Go code in [internal/proto](internal/proto) as part of that PR, so
that the Go client in the tag matches the server.

After that PR being merged, an annotated tag can be pushed to the repository or
via the GitHub UI. We usually auto-generate the changelog directly from the
GitHub UI. A ["Create Release" GitHub
action](https://github.com/containers/conmon-rs/actions/workflows/release.yml)
should run for that tag which attaches the sources, the static binaries and
their checksums to the release, for example
`https://github.com/containers/conmon-rs/releases/download/vX.Y.Z/conmonrs-vX.Y.Z.tar.gz`.
It also points the `latest-X.Y.txt` marker in the bucket to the tag. The
tarball is a `git archive` of the tag, so it has the same content as the
automatic GitHub archive which [rpm/conmon-rs.spec](rpm/conmon-rs.spec) uses as
`Source0` (packit replaces that with an archive of the commit for Copr builds).

The same tag also versions the Go module `github.com/containers/conmon-rs`, so
consumers like CRI-O pick the release up with `go get
github.com/containers/conmon-rs@vX.Y.Z`. The Go module has no separate
versioning, which means the Go client must stay backwards compatible within a
major version.

Those sources have to be used to update the package in
[brew](https://brewweb.engineering.redhat.com/brew). To update the package, run
in an appropriate environment:

```shell
> kinit $USER@IPA.REDHAT.COM
> rhpkg clone conmon-rs
> cd conmon-rs
> git checkout rhaos-X.Y-rhel-N  # the target OpenShift and RHEL release
> vim conmon-rs.spec  # edit the Version
> spectool -g conmon-rs.spec
> rhpkg new-sources *.tar.gz
> rpmdev-bumpspec -c "Bump to vX.Y.Z" conmon-rs.spec
> git commit -asm "Bump to vX.Y.Z"
> git push
> rhpkg build
```
