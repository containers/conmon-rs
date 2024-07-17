{ pkgs }:
with pkgs; rustPlatform.buildRustPackage {
  name = "conmon-rs";
  # Use Pure to avoid exuding the .git directory
  src = nix-gitignore.gitignoreSourcePure [ ../.gitignore ] ./..;
  doCheck = false;
  nativeBuildInputs = with buildPackages; [
    capnproto
    protobuf
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
  ] ++ lib.optionals stdenv.hostPlatform.isMusl [
    "-Cpanic=abort"
    "-Clink-args=-nostartfiles"
    "-Clink-args=-L${libunwind}/lib"
  ];
  # Patch nix v0.27.1 for musl
  preBuild = lib.optionalString stdenv.hostPlatform.isMusl ''
    sed -i 's;target_arch = "s390x";target_arch = "s390x" , not(target_env = "musl");g' \
      /build/cargo-vendor-dir/nix-0.27.1/src/sys/statfs.rs
  '';
  stripAllList = [ "bin" ];
  cargoLock = {
    lockFile = lib.cleanSource ./.. + "/Cargo.lock";
    allowBuiltinFetchGit = true;
  };
}

