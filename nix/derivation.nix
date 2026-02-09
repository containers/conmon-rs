{ pkgs }:
with pkgs; rustPlatform.buildRustPackage {
  name = "conmon-rs";
  # Use Pure to avoid exuding the .git directory
  src = nix-gitignore.gitignoreSourcePure [ ../.gitignore ] ./..;
  doCheck = false;
  nativeBuildInputs = with buildPackages; [
    capnproto
    gitMinimal
    protobuf
  ];
  buildInputs =
    if stdenv.hostPlatform.isMusl then [
      libunwind
    ] else [
      glibc
      glibc.static
    ];
  # Fix nix crate statfs type mismatch on s390x-musl
  postPatch = lib.optionalString (stdenv.hostPlatform.isS390x && stdenv.hostPlatform.isMusl) ''
    for f in /build/cargo-vendor-dir/nix-*/src/sys/statfs.rs; do
      if [ -f "$f" ]; then
        chmod +w "$f"
        sed -i 's/self\.0\.f_\([a-z_]*\)$/self.0.f_\1 as _/' "$f"
        sed -i 's/self\.0\.f_\([a-z_]*\))/self.0.f_\1 as _)/' "$f"
      fi
    done
  '';
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

