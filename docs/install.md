# Installation

## Obtain the latest version

We provide statically linked binaries for every successfully built commit on
`main` via our [Google Cloud Storage Bucket][bucket], for `amd64`, `arm64`,
`ppc64le` and `s390x`. Our provided [get script](../scripts/get) can be used to
download the latest version:

```console
> curl -fsSL https://raw.githubusercontent.com/containers/conmon-rs/main/scripts/get | bash
```

It is also possible to select a specific commit by its full 40 character SHA
(`-t`), a release tag like `vX.Y.Z` (`-l`), the architecture (`-a`, defaults to
the local one) or the output binary path (`-o`), and to skip running the
installed binary for its version (`-n`). With `-l`, the script resolves the tag
through the GitHub API, which needs [`jq`](https://jqlang.org):

```console
> curl -fsSL https://raw.githubusercontent.com/containers/conmon-rs/main/scripts/get | \
    bash -s -- -l vX.Y.Z -a arm64 -o $OUTPUT_PATH -n
```

The script verifies the sigstore signature of the binary if the local system
has [`cosign`](https://github.com/sigstore/cosign) available in its `$PATH`.
That needs cosign v2.4.2 or newer, older versions cannot read the bundles of the
signatures and fail. The signature has to come from the ci workflow run of
exactly that commit on `main`. Without cosign, the script only checks the
SHA-256 checksum, the one of `sha256sums.txt` for a release and the one in the
bucket otherwise, which only shows that the download is complete. It fails if no
checksum got published, like for old commits and the releases before v1.1.0,
unless `-k` is given.

Without `-t` and `-l`, the script checks with the GitHub API that the latest
commit of the bucket is on `main`, and warns if it is more than a day older than
the head of `main`, like when the bucket points back to an old commit. Without
`jq`, or if the API fails, like because of its rate limit (`GITHUB_TOKEN` raises
it), the script only warns.

Every commit on `main` is also built as an RPM in the [podman-next
copr](https://copr.fedorainfracloud.org/coprs/rhcontainerbot/podman-next/package/conmon-rs).

[bucket]: https://console.cloud.google.com/storage/browser/cri-o/conmon-rs

## Verify the downloads

The bucket serves the files of a commit at
`https://storage.googleapis.com/cri-o/conmon-rs/$COMMIT/`, `latest-main.txt`
names the latest commit of `main` whose binaries got published and whose tests
passed, and `latest-X.Y.txt` the latest release tag of a minor version. Since
v1.1.0, the [GitHub releases](https://github.com/containers/conmon-rs/releases)
carry the same binaries as `conmonrs.$ARCH` with their signatures, built from
the commit of the tag. To verify a binary by hand, download it with its files and set
`COMMIT` to that commit (the assignments `COMMIT=$(...)` need a POSIX shell
like bash or zsh), either for a release:

```console
> gh release download vX.Y.Z --repo containers/conmon-rs \
    --pattern 'conmonrs.amd64*'
> COMMIT=$(gh api repos/containers/conmon-rs/commits/vX.Y.Z --jq .sha)
```

or for the latest commit of `main` in the bucket:

```console
> COMMIT=$(curl -fsSL https://storage.googleapis.com/cri-o/conmon-rs/latest-main.txt)
> curl -fsSL --remote-name-all \
    "https://storage.googleapis.com/cri-o/conmon-rs/$COMMIT/conmonrs.amd64{,.bundle,.intoto.jsonl,.spdx.json,.spdx.intoto.jsonl,.vex.json,.vex.intoto.jsonl}"
```

curl expands the braces itself. For an older commit, it reports a 404 for
each of these files the commit does not have and exits with an error, but
still downloads the others. Then check the signature, which the publish-static
workflow made (use `ci.yml` instead of `publish-static.yml` for older commits
without attestations, like the ones of v1.1.0 and v1.1.1):

```console
> cosign verify-blob conmonrs.amd64 \
    --bundle conmonrs.amd64.bundle \
    --certificate-identity https://github.com/containers/conmon-rs/.github/workflows/publish-static.yml@refs/heads/main \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    --certificate-github-workflow-sha "$COMMIT"
```

The binaries of newer commits on `main`, and of the releases since v1.2.0,
also come with [SLSA](https://slsa.dev) Build L3 provenance and an SPDX 2.3
SBOM, both as GitHub artifact attestations. A job without access to the
signing credentials builds them, the reusable [publish-static
workflow](../.github/workflows/publish-static.yml), which runs no build steps,
attests and signs them. Verify them with the [GitHub
CLI](https://cli.github.com), which also checks that they got built from
`$COMMIT`:

```console
> gh attestation verify conmonrs.amd64 \
    --repo containers/conmon-rs \
    --signer-workflow containers/conmon-rs/.github/workflows/publish-static.yml \
    --source-ref refs/heads/main \
    --source-digest "$COMMIT"
```

Add `--predicate-type https://spdx.dev/Document/v2.3` for the SBOM. The
binaries of newer commits, and of the releases since v1.2.1, also come with an
[OpenVEX](https://openvex.dev) document as an attestation, which states how
known vulnerabilities of the crates of the SBOM affect conmon-rs. It is the
document [vex/conmonrs.openvex.json](../vex/conmonrs.openvex.json) of the
commit, [release.md](../release.md#vulnerabilities-and-vex-statements)
describes its statements. Verify it and print the attested document with:

```console
> gh attestation verify conmonrs.amd64 \
    --repo containers/conmon-rs \
    --signer-workflow containers/conmon-rs/.github/workflows/publish-static.yml \
    --source-ref refs/heads/main \
    --source-digest "$COMMIT" \
    --predicate-type https://openvex.dev/ns/v0.2.0 \
    --format json --jq '.[0].verificationResult.statement.predicate'
```

The bucket and the releases carry the attestation bundles as well
(`conmonrs.$ARCH.intoto.jsonl`, `conmonrs.$ARCH.spdx.intoto.jsonl` and
`conmonrs.$ARCH.vex.intoto.jsonl`), and the SBOM and the VEX document
themselves (`conmonrs.$ARCH.spdx.json` and `conmonrs.$ARCH.vex.json`).
`gh attestation verify` takes a bundle with `--bundle` and the matching
`--predicate-type` instead of fetching the attestations from GitHub.

### Scan with the VEX document

The statements of the VEX document name conmon-rs
(`pkg:github/containers/conmon-rs`) as their product and the affected crates
as its subcomponents. Neither [grype](https://github.com/anchore/grype) nor
[trivy](https://trivy.dev) applies such a statement to the SBOM of the binary:
grype only matches a statement whose product is the vulnerable crate itself,
trivy does not know the `pkg:github` type, and both ignore the statements
silently. Make the subcomponents the products first, which both apply:

```console
> jq '.statements |= map(.products = [.products[].subcomponents[]])' \
    conmonrs.amd64.vex.json >crates.vex.json
> grype sbom:conmonrs.amd64.spdx.json --vex crates.vex.json
> trivy sbom conmonrs.amd64.spdx.json --vex crates.vex.json
```

`make scan SBOMS=conmonrs.amd64.spdx.json` does the same with grype and the
VEX document of the repository.

### Verify the source tarball

Since v1.2.0, the source tarball and the `sha256sums.txt` of a release have
SLSA Build L3 provenance (also as the bundle `sources.intoto.jsonl`) and cosign
signatures (`conmonrs-vX.Y.Z.tar.gz.bundle` and `sha256sums.txt.bundle`) of
the reusable [build-sources
workflow](../.github/workflows/build-sources.yml) for the tag. `sha256sums.txt`
lists the checksums of the tarball and the binaries. `COMMIT` is the commit of
the tag, like for the binaries of a release:

```console
> gh release download vX.Y.Z --repo containers/conmon-rs \
    --pattern conmonrs-vX.Y.Z.tar.gz --pattern 'sha256sums.txt*' \
    --pattern sources.intoto.jsonl
> COMMIT=$(gh api repos/containers/conmon-rs/commits/vX.Y.Z --jq .sha)
> gh attestation verify conmonrs-vX.Y.Z.tar.gz \
    --bundle sources.intoto.jsonl \
    --repo containers/conmon-rs \
    --signer-workflow containers/conmon-rs/.github/workflows/build-sources.yml \
    --source-ref refs/tags/vX.Y.Z \
    --source-digest "$COMMIT"
> cosign verify-blob sha256sums.txt \
    --bundle sha256sums.txt.bundle \
    --certificate-identity https://github.com/containers/conmon-rs/.github/workflows/build-sources.yml@refs/tags/vX.Y.Z \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    --certificate-github-workflow-sha "$COMMIT"
> sha256sum --ignore-missing -c sha256sums.txt
```

## Build from source

Building the server needs the Cap'n Proto compiler (`capnp`, packaged as
`capnproto`) and a Rust toolchain of at least the `rust-version` of
[Cargo.toml](../Cargo.toml). To build and install a release binary:

```console
> make release
> sudo make install PREFIX=/usr/local
```

Packages built from a source tarball, which has no git metadata, pass in what
`--version` shows: `BUILD_TAG=vX.Y.Z` or `BUILD_COMMIT=<sha>`, either one wins
over git and leaves the other one empty, and `SOURCE_DATE_EPOCH` for the build
date, which is the time of the build otherwise. The dependency tree of
`--version full` comes from `cargo tree --locked --offline` during the build,
which needs the crates of [Cargo.lock](../Cargo.lock) present, like after
`cargo fetch` or `cargo vendor`, the tree stays empty otherwise.
`CONMONRS_SKIP_CARGO_TREE` leaves it empty and skips that step. See
[conmon-rs/server/build.rs](../conmon-rs/server/build.rs), and
[rpm/conmon-rs.spec](../rpm/conmon-rs.spec) for how the RPM passes them.
