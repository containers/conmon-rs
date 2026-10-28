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

