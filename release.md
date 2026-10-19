# Creating a new release

Creating a new conmon-rs release can be done by proposing a PR to bump the
versions in the following files:

- [conmon-rs/common/Cargo.toml](conmon-rs/common/Cargo.toml) (`version`)
- [conmon-rs/server/Cargo.toml](conmon-rs/server/Cargo.toml) (`version` and `conmon-common` dependency version)
- [conmon-rs/client/Cargo.toml](conmon-rs/client/Cargo.toml) (`version` and `conmon-common` dependency version)

If the Cap'n Proto schema in
[conmon-rs/common/proto/conmon.capnp](conmon-rs/common/proto/conmon.capnp)
changed since the last release, run `make update-proto` and commit the
regenerated Go code in [internal/proto](internal/proto) as part of that PR, so
that the Go client in the tag matches the server.

After that PR being merged, an annotated tag can be pushed to the repository or
via the GitHub UI. We usually auto-generate the changelog directly from the
GitHub UI. A ["Create Release" GitHub
action](https://github.com/containers/conmon-rs/actions/workflows/release.yml)
should run for that tag which attaches the vendored sources to the release, for
example:

- [conmonrs-v0.4.0.tar.gz](https://github.com/containers/conmon-rs/releases/download/v0.4.0/conmonrs-v0.4.0.tar.gz)

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
> git checkout rhaos-4.12-rhel-8  # or rhaos-4.12-rhel-9
> vim conmon-rs.spec  # edit the Version
> spectool -g conmon-rs.spec
> rhpkg new-sources *.tar.gz
> rpmdev-bumpspec -c "Bump to v0.5.0" conmon-rs.spec
> git commit -asm "Bump to v0.5.0"
> git push
> rhpkg build
```
