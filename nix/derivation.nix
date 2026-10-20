{ pkgs, rev, lastModified }:
with pkgs; rustPlatform.buildRustPackage {
  name = "conmon-rs";
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
  ] ++ lib.optionals stdenv.hostPlatform.isMusl [
    "-Cpanic=abort"
    "-Clink-args=-nostartfiles"
    "-Clink-args=-L${libunwind}/lib"
  ];
  stripAllList = [ "bin" ];
  cargoLock = {
    lockFile = lib.cleanSource ./.. + "/Cargo.lock";
    allowBuiltinFetchGit = true;
  };
}

