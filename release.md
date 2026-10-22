# Creating a new release

Creating a new conmon-rs release can be done by proposing a PR to bump the
`version` in the `[workspace.package]` section of [Cargo.toml](Cargo.toml)
and the `Version` in [rpm/conmon-rs.spec](rpm/conmon-rs.spec), and running
`cargo update --workspace` to update [Cargo.lock](Cargo.lock).

After that PR being merged, an annotated tag can be pushed to the repository or
via the GitHub UI. We usually auto-generate the changelog directly from the
GitHub UI. The [release
workflow](https://github.com/containers/conmon-rs/actions/workflows/release.yml)
should run for that tag which attaches the sources, the static binaries, their
checksums, signatures, SLSA provenance and SBOMs to the release, for example
`https://github.com/containers/conmon-rs/releases/download/vX.Y.Z/conmonrs-vX.Y.Z.tar.gz`.
Its build-sources workflow builds the tarball and the checksums in a job
without the signing credentials, and attests and signs them in a separate one.
The release job verifies all files once more before publishing them. The
workflow also points the `latest-X.Y.txt` marker in the bucket to the tag. The
tarball is a `git archive` of the tag, so it has the same content as the
automatic GitHub archive which [rpm/conmon-rs.spec](rpm/conmon-rs.spec) uses as
`Source0` (packit replaces that with an archive of the commit for Copr builds).

The release takes the static binaries which the ci workflow built, attested
and signed when the commit reached main, so the tag has to point to a commit
which had its own ci run on main, like the merge commit of the version bump PR.
The release fails right away otherwise, and for commits from before the builds
got attested. Those binaries got built before the tag existed, the version and
commit identify them and their tag stays empty.

The same tag also versions the Go module `github.com/containers/conmon-rs`, so
consumers like CRI-O pick the release up with `go get
github.com/containers/conmon-rs@vX.Y.Z`. The Go module has no separate
versioning, which means the Go client must stay backwards compatible within a
major version.

Red Hat maintainers then update the package in the Red Hat internal
[brew](https://brewweb.engineering.redhat.com/brew) with those sources. To
update the package, run in an environment with access to it:

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
