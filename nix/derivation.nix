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
  stripAllList = [ "bin" ];
  cargoVendorDir = ".cargo-vendor";
  cargoLock = {
    lockFile = lib.cleanSource ./.. + "/Cargo.lock";
    allowBuiltinFetchGit = true;
  };
}

