{ pkgs, rev, lastModified }:
with pkgs; rustPlatform.buildRustPackage {
  pname = "conmon-rs";
  inherit ((lib.importTOML ../Cargo.toml).workspace.package) version;
  # The source has no .git directory, pass the build metadata in.
  BUILD_COMMIT = rev;
  SOURCE_DATE_EPOCH = lastModified;
  # Use Pure to avoid exuding the .git directory
  src = nix-gitignore.gitignoreSourcePure [ ../.gitignore ] ./..;
  doCheck = false;
  nativeBuildInputs = with buildPackages; [
    capnproto
  ];
  buildInputs =
    if stdenv.hostPlatform.isMusl then [
      libunwind
    ] else [
      glibc
      glibc.static
    ];
  RUSTFLAGS = [
    "-Ctarget-feature=+crt-static"
    # With panic = "abort" and the symbols stripped, the unwind tables of the
    # crates would only serve backtraces without symbols. The ones of the
    # prebuilt standard library stay. The tests do not build here.
    "-Cforce-unwind-tables=no"
  ] ++ lib.optionals (!stdenv.hostPlatform.isMusl) [
    # Packs the relative relocations of the static PIE into .relr.dyn, which
    # shrinks the file and the relocation table read at every start. The
    # relocated pages get written either way. glibc >= 2.36 handles them in
    # static PIE, which the nixpkgs one does. Not for make release, a dynamic
    # binary would require a glibc with DT_RELR support at runtime. A no-op
    # on ppc64le, which links non-PIE.
    "-Clink-arg=-Wl,-z,pack-relative-relocs"
  ] ++ lib.optionals stdenv.hostPlatform.isPower64 [
    # Rust links static PIE on ppc64le, but the glibc of nixpkgs has no
    # rcrt1.o there.
    "-Crelocation-model=static"
  ] ++ lib.optionals stdenv.hostPlatform.isMusl [
    "-Clink-args=-nostartfiles"
    "-Clink-args=-L${libunwind}/lib"
  ];
  stripAllList = [ "bin" ];
  cargoLock = {
    lockFile = lib.cleanSource ./.. + "/Cargo.lock";
  };
}

