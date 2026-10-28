# Creating a new release

Creating a new conmon-rs release starts with a PR which bumps the version of
the `conmon-rs` entry in [dependencies.yaml](dependencies.yaml) and in the
files it lists, the `version` in the `[workspace.package]` section of
[Cargo.toml](Cargo.toml) and the `Version` in
[rpm/conmon-rs.spec](rpm/conmon-rs.spec). The PR also runs
`cargo update --workspace` to update [Cargo.lock](Cargo.lock) and replaces
"unreleased" in [usage.md](usage.md) with the version.
`make verify-dependencies verify-spec` has to pass.

After that PR got merged, push an annotated tag for its merge commit with git:

```shell
> git fetch git@github.com:containers/conmon-rs.git main
> COMMIT=$(gh pr view <pr-number> --repo containers/conmon-rs --json mergeCommit --jq .mergeCommit.oid)
> git tag -a vX.Y.Z -m vX.Y.Z $COMMIT
> git push git@github.com:containers/conmon-rs.git vX.Y.Z
```

Only `vX.Y.Z` tags get released, there are no prereleases: the crate version
has to match the tag and the RPM `Version` cannot have a dash. The workflow
fails right away for a tag with a suffix like `vX.Y.Z-rc.1`, which cannot be
deleted afterwards, so do not push one.

Do not create the release or the tag in the GitHub UI, and never publish a
release before the workflow did. The [release
workflow](https://github.com/containers/conmon-rs/actions/workflows/release.yml)
runs for the tag, creates a draft release with the notes GitHub generates,
attaches the sources, the static binaries, their checksums, signatures, SLSA
provenance, SBOMs and VEX documents to it, for example
`https://github.com/containers/conmon-rs/releases/download/vX.Y.Z/conmonrs-vX.Y.Z.tar.gz`,
and publishes it only then, so a published release always has all files and
can be immutable. Edit the release notes afterwards. A release only becomes
the latest one if no other release has a higher version, so a patch release of
an older minor does not. A rerun continues with the draft, and leaves a
published release alone if it has all files.

Its build-sources workflow builds the tarball and the checksums in a job
without the signing credentials, and attests and signs them in a separate one.
The release job verifies all files once more before publishing them. The
workflow also points the `latest-X.Y.txt` marker in the bucket to the tag, but
only forward, not back to an older patch release. The tarball is a
`git archive` of the tag, so it has the same content as the automatic GitHub
archive which [rpm/conmon-rs.spec](rpm/conmon-rs.spec) uses as `Source0`
(packit replaces that with an archive of the commit for Copr builds).

The repository protects the `v*` tags: nobody can move or delete them, and only
maintainers can create them. Tag a new version instead of fixing a released one.

The release takes the static binaries which the ci workflow built, attested
and signed when the commit reached main, so the tag has to point to a commit
which had its own ci run on main, like the merge commit of the version bump PR.
The release fails right away otherwise, and for commits from before the builds
got attested or before the VEX document
[vex/conmonrs.openvex.json](vex/conmonrs.openvex.json) existed. Those binaries
got built before the tag existed, the version and commit identify them and
their tag stays empty.

The release waits for that ci run and fails unless all of its jobs besides
the publish ones passed in its latest attempt, so a commit whose tests or
checks failed, like the vulnerability scans, does not get released. Wait for
the ci run of the merge commit to pass before pushing the tag. If it fails
anyway, "Re-run failed jobs" of the ci run and then rerun the release
workflow. A failure which a rerun cannot fix needs a new version bump PR for
the next patch version once the cause is fixed: the tag cannot move to
another commit, and the crate version has to match the tag.

Use only "Re-run failed jobs" for a ci run on main, never "Re-run all jobs".
The latter builds the binaries again, and if they do not have the same bits,
the publish-static jobs refuse to replace the signed ones in the bucket and
fail. A release still takes the binaries of the earlier attempt as long as
the other jobs pass, but the run stays failed. A cancelled publish-marker job
is fine: the runs of main publish the `latest-main.txt` marker one at a time,
and a job which queues later replaces one which waits. The marker only moves
forward, at worst it stays behind until the run of the next commit.

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

## Vulnerabilities and VEX statements

The ci workflow checks every change for known vulnerabilities: `verify-deny`
checks the crates of [Cargo.lock](Cargo.lock) against the RustSec advisories
with cargo-deny, `verify-vulnerabilities` scans the SBOMs of the static
binaries with grype (`make scan`) and fails for High and Critical
vulnerabilities which have a fix, and `verify-go-vulnerabilities` reports the
findings of govulncheck for the Go client as warnings, since its dependencies
follow CRI-O. The [advisories
workflow](https://github.com/containers/conmon-rs/actions/workflows/advisories.yml)
runs these checks every day against main and the attested SBOMs of the
binaries of `latest-main.txt`, and fails for their findings, the ones of
govulncheck included.

If a vulnerability does not affect conmon-rs, like because it does not call
the vulnerable code, add a statement to the OpenVEX document
[vex/conmonrs.openvex.json](vex/conmonrs.openvex.json). The publish-static
workflow attests it for every binary, and the scans ignore the vulnerability
for the crates the statement lists. A statement names
`pkg:github/containers/conmon-rs` as its product, without a version since the
document belongs to the binaries of its commit, and the affected crates as its
subcomponents (`pkg:cargo/<crate>`, with `@<version>` to limit it to a
version), and lists the GHSA ID of the vulnerability, which grype reports, as
its name or as an alias. Its status is `not_affected` with a justification
(`vulnerable_code_not_present`, `vulnerable_code_not_in_execute_path`,
`vulnerable_code_cannot_be_controlled_by_adversary`,
`inline_mitigations_already_exist` or `component_not_present`) or an
`impact_statement` which explains why, `affected` with an `action_statement`,
`fixed` or `under_investigation`. Only `not_affected` and `fixed` hide a
finding. An `affected` statement informs the users of the binaries, the scans
keep failing for a finding with a fix until the crate gets updated or the
statement becomes `not_affected` or `fixed`. vexctl adds a statement, raises
the `version` of the document and sets its `last_updated`, which a statement
added by hand has to do as well.
vexctl dates the statement with `SOURCE_DATE_EPOCH` if set, like in a nix
shell, unset it first:

```shell
> go run github.com/openvex/vexctl@v0.4.4 add --in-place vex/conmonrs.openvex.json \
    --product pkg:github/containers/conmon-rs \
    --subcomponents pkg:cargo/<crate> \
    --vuln GHSA-xxxx-xxxx-xxxx \
    --aliases CVE-YYYY-NNNNN,RUSTSEC-YYYY-NNNN \
    --status not_affected \
    --justification vulnerable_code_not_in_execute_path \
    --impact-statement "Why conmon-rs is not affected."
> make prettier verify-vex
> make scan SBOMS=conmonrs.amd64.spdx.json
```

For an SBOM of a file, grype only applies a statement whose product is the
vulnerable crate itself, so `make scan` makes the subcomponents of the
statements their products. A released binary keeps the document it got
attested with, a new statement only reaches the binaries built after it. The
daily scan uses the document of main.
