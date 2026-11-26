# AGENTS.md

conmon-rs is a pod level OCI container monitor: the Rust server `conmonrs`
(one process per pod, Cap'n Proto RPC on a UNIX socket) and the Go client in
`pkg/client`, which CRI-O vendors. The server targets Linux only; on other
hosts only the Go client builds. Read
[docs/architecture.md](docs/architecture.md) first,
[CONTRIBUTING.md](CONTRIBUTING.md) has the details behind this file.

## Layout

| Path                                         | What                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------ |
| `conmon-rs/server/`                          | The only shipped crate, `conmonrs` (lib + bin, not published)            |
| `conmon-rs/server/proto/conmon.capnp`        | RPC schema, the source of truth for server and client                    |
| `conmon-rs/server/proto/conmon.capnp.layout` | Generated wire layout snapshot, committed and checked                    |
| `conmon-rs/server/proto/go-patch`            | Go annotations prepended to the schema for the Go code generation        |
| `internal/proto/conmon.capnp.go`             | Generated Go code, committed                                             |
| `pkg/client/`                                | Go client, part of the root module `github.com/containers/conmon-rs`     |
| `pkg/client/integration/`                    | Ginkgo suite, a separate Go module (`replace` to the checkout)           |
| `fuzz/`                                      | cargo-fuzz targets, a workspace member but not a default member          |
| `flake.nix`, `nix/`                          | Static release builds (glibc, musl for s390x)                            |
| `.github/verify-*`, `.github/changes`        | Scripts behind the `make verify-*` targets and the CI path filter        |
| `dependencies.yaml`, `rpm/`, `vex/`          | Version tracking (zeitgeist), RPM spec, OpenVEX document of the binaries |

## Server modules (`conmon-rs/server/src`)

| Module                                       | Owns                                                                          |
| -------------------------------------------- | ----------------------------------------------------------------------------- |
| `rpc.rs`                                     | The `Conmon` interface, one method per RPC                                    |
| `server.rs`, `init.rs`, `listener.rs`        | Start, fork, umask/OOM score/descriptor setup, sockets, shutdown              |
| `config.rs`                                  | clap CLI and validation, documented in `docs/server.md`                       |
| `child_reaper/`, `process.rs`                | Runtime calls with timeouts, watchers, orphan sweep, pidfd based reaping      |
| `runtime_args.rs`, `exec_start.rs`           | OCI runtime command lines (create, restore, exec), starting execs             |
| `container_io/`, `terminal.rs`, `channel.rs` | Pipes or pty (console socket, `SCM_RIGHTS`), read loops, post-exit drain      |
| `container_log/`                             | Log drivers: `cri`, `journald`, `log_file`, `line_buffer`, blocking admission |
| `journal.rs`                                 | Native journald protocol client (no libsystemd)                               |
| `attach.rs`                                  | Per container attach socket, the conmon protocol                              |
| `streaming_server/`                          | Exec, attach and port forward over WebSocket and SPDY/3.1, shared budgets     |
| `oom_watcher/`                               | cgroup v2 `memory.events` through one shared inotify (cgroup v1 got removed)  |
| `telemetry.rs`                               | OpenTelemetry, only with the `tracing` feature (not in the shipped binary)    |
| `version.rs`                                 | `--version`, the `version` RPC and the feature strings                        |
| `mutex.rs`, `fs_util.rs`, `util.rs`          | Poison tolerant locks, symlink safe file helpers, shared helpers              |
| `*/fuzzing.rs`, `fuzzing/tests.rs`           | Fuzz entry points, exported as `conmonrs::fuzzing`                            |

## Server invariants reviewers check

- **One thread.** A tokio `current_thread` runtime runs everything;
  `deny.toml` bans tokio's `rt-multi-thread` outside of tests. Never block
  it, file I/O goes to the bounded blocking pool (`container_log/blocking.rs`).
- **Memory is budgeted.** An idle server has about 0.5 MiB of private memory,
  the integration specs fail above `MAX_RSS_KB`, `MAX_RSS_ANON_KB` and
  `MAX_RSS_GROWTH_KB` (see `Makefile`, CI sets tighter values). Avoid per
  container buffers and tasks. New dependencies take
  `default-features = false` like the existing ones of the server, and
  `deny.toml` rejects duplicate crate versions.
- **Bound everything a client or container controls**: queues, buffers,
  waits, sessions. Keep permits and reservations held through cancellation.
  A recurring fix class, see `git log -i --grep bound`.
- **PID reuse.** Signal and wait through the pidfd of the registration
  (`process.rs`, `child_reaper/`), never by a bare PID; a PID a runtime
  reports has to be an unreaped child of the server.
- **No symlink following.** Exit, OOM and log files are created without
  following symlinks; sockets are removed only if `fs_util::FileId` still
  matches, so an old server cannot unlink a replacement's socket.
- **Panics abort** release builds (`panic = "abort"`), a panic in any task
  kills the server and its pod monitoring.
- Lints: `unsafe_code = "deny"`; the few exceptions use
  `#[expect(unsafe_code, reason = "...")]` plus a `SAFETY` comment. Use
  `#[expect(..., reason = "...")]`, not `#[allow]`, and `pub(crate)` for items
  (`unreachable_pub`). See `[workspace.lints]` in `Cargo.toml`.

## Cap'n Proto schema

- Rust: `conmon-rs/server/build.rs` compiles the schema into `OUT_DIR` on
  every build (nothing committed), so any build needs `capnp` on the `PATH`.
- Go: `make update-proto` concatenates the schema and `go-patch`, installs
  `capnpc-go` at the version of `go.mod` and runs the `capnp` pinned by
  `flake.lock` through nix. Commit `internal/proto/conmon.capnp.go`.
  `CAPNP=capnp` uses a local compiler, but the generated code embeds the
  schema, which differs between capnp versions, so CI may reject it.
- Commit additions and renames to the layout with `make update-schema-layout`.
- Wire rules (full list in
  [CONTRIBUTING.md](CONTRIBUTING.md#changing-the-schema)): only add fields,
  methods and enumerants with the next ordinal; never change a type or
  ordinal, never remove; retire by renaming to `fooOld`. Servers outlive
  CRI-O upgrades, so old clients talk to new servers and the other way
  around, see [docs/cri-o.md](docs/cri-o.md#compatibility). CI runs critest
  with the client vendored by the CRI-O release of `test-critest` in `ci.yml`.
- Behavior a client must detect gets a feature string in `version.rs`
  (`FEATURES` and its `version_features` test), a `Feature*` constant in
  `pkg/client/version.go`, a row in
  [docs/protocols.md](docs/protocols.md#features), and a client check that
  fails with `ErrUnsupported`.
- The error message prefixes listed in the comment of `interface Conmon` are
  API: `pkg/client/errors.go` matches them (`container not found`). Do not
  reword them.

## Go client API

- The repository tag versions the Go module and CRI-O vendors it, so
  `pkg/client` stays compatible within a major version. `make verify-go-api`
  runs gorelease against the highest `vX.Y.Z` tag reachable from HEAD, in a
  worktree of HEAD: commit first, fetch the tags
  (`git fetch --tags https://github.com/containers/conmon-rs.git`), and it
  needs the Go module proxy. Keep changes additive. A break needs a new
  major version or an exception in `.github/go-api-break`, which is tied to
  one release tag, see release.md. Ask the maintainers before breaking it.
- CRI-O builds the client for FreeBSD too: CI runs `go build` and `go vet`
  with `CGO_ENABLED=0` for `GOOS=freebsd` and `darwin`. Keep Linux code in
  `*_linux.go` with stubs in `client_other.go`.
- The dependencies of the root `go.mod` follow CRI-O main, do not bump them
  for the integration suite, whose module has its own. CI runs
  `go mod tidy -diff` and `go mod verify` in both modules.
- Tests of package `client` are `*_internal_test.go` (the `testpackage`
  linter), `.golangci.yml` enables about 100 linters for both modules.

## Build, test, lint

`make help` lists the targets, and
[CONTRIBUTING.md](CONTRIBUTING.md#building-and-testing) explains them. The
usual loop is `make`, `make unit`, `make unit-go`, `make fmt` and `make lint`;
`make verify` runs the other CI checks, several of which need containers,
nix, network or extra tools. `.github/workflows/ci.yml` is the reference for
what CI runs.

- CI pins its toolchain in `RUST_VERSION` of `.github/workflows/ci.yml`,
  so lints of a newer local toolchain do not belong in unrelated PRs.
- Integration: build and install ginkgo as your user, then run the suite as
  root with your Go environment, exactly as in
  [CONTRIBUTING.md](CONTRIBUTING.md#building-and-testing). `CONMON_BINARY`
  picks the server (default: builds `target/release/conmonrs`),
  `RUNTIME_PATH` the runtime (default `/usr/bin/runc`). OOM, port forward,
  checkpoint and journald specs skip without root, cgroup v2, criu (or a
  crun with `+CRIU`, which the static crun lacks) or a journal.
- Rust tests live in `<module>/tests.rs` (`#[cfg(test)] mod tests;`) with
  helpers in `test_util.rs` and `*/testing.rs`. Use
  `#[tokio::test(start_paused = true)]` or events instead of sleeps. CI also
  runs them on arm64 and under qemu on ppc64le and big-endian s390x, so
  hand-encoded integers must be endian-correct and timing slack generous.
- New fuzz target: `fuzz/fuzz_targets/<name>.rs`, a `[[bin]]` in
  `fuzz/Cargo.toml`, the entry point in `conmonrs::fuzzing` (`lib.rs`), an
  entry in `TARGETS` of `src/fuzzing/tests.rs` and seeds in
  `fuzz/seeds/<name>/`; the unit tests fail otherwise. Running needs nightly:
  `cargo +nightly fuzz run <name> fuzz/corpus/<name> fuzz/seeds/<name>`.

## CI checks a PR commonly trips

- `.github/verify-panics`: a passing test printed a panic from a spawned
  task. Fix the task, the release build would abort.
- `verify-deny`: duplicate crate version, license, advisory or yanked crate.
  Bump or replace the crate. The VEX document only filters the grype scans,
  cargo-deny does not read it.
- `verify-dependencies`, `verify-spec`: a tool or crate version changed in
  one place only. Update `dependencies.yaml` and `rpm/conmon-rs.spec` too.
- The schema and Go API checks are covered above.

PRs which only touch Markdown files and `docs/` (outside `.github`) skip
the ci.yml jobs besides `changes` and `lint-files`, see `.github/changes`.

## Documentation

- User-visible changes update the matching document in `docs/` in the same
  PR: `server.md` (the flag table mirrors `config.rs`), `protocols.md`,
  `logging.md`, `troubleshooting.md`, `cri-o.md`.
- Mark new fields, limits and behavior with "since unreleased" in the schema
  comments, the Go documentation and `docs/`, and add a line starting with
  "Unreleased:" at the top of the "Changes" section of the document. The
  version bump replaces the markers, `make verify-unreleased` checks them.
- A new server flag the client passes also goes into `pkg/client/client.go`.

## Commits and PRs

- Sign off every commit (`git commit -s`), DCO.
- Subjects are imperative and capitalized without a prefix ("Bound active
  streaming sessions"); bodies explain why, bullet lists for several changes.
  Fixes come with regression tests.
- Use `.github/PULL_REQUEST_TEMPLATE.md`: one `/kind`, `Fixes #`, and the
  `release-note` block (`None` without a user-facing change).
- Prow merges approved PRs (`OWNERS`). Version bumps and tags follow
  [release.md](release.md) and are not part of regular PRs.
