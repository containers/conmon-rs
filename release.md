# Creating a new release

Creating a new conmon-rs release can be done by proposing a PR to bump the
versions in the following files:

- [conmon-rs/common/Cargo.toml](conmon-rs/common/Cargo.toml) (`version`)
- [conmon-rs/server/Cargo.toml](conmon-rs/server/Cargo.toml) (`version` and `conmon-common` dependency version)
- [conmon-rs/client/Cargo.toml](conmon-rs/client/Cargo.toml) (`version` and `conmon-common` dependency version)

After that PR being merged, an annotated tag can be pushed to the repository or
via the GitHub UI. We usually auto-generate the changelog directly from the
GitHub UI. A ["Create Release" GitHub
action](https://github.com/containers/conmon-rs/actions/workflows/release.yml)
should run for that tag which attaches the vendored sources to the release, for
example:

- [conmonrs-v0.4.0.tar.gz](https://github.com/containers/conmon-rs/releases/download/v0.4.0/conmonrs-v0.4.0.tar.gz)

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
