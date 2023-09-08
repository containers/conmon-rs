{ pkgs }:
with pkgs; rustPlatform.buildRustPackage {
  name = "conmon-rs";
  src = ./..;
  doCheck = false;
  nativeBuildInputs = with buildPackages; [
    capnproto
    protobuf
  ];
  buildInputs = [
    glibc
    glibc.static
  ];
  RUSTFLAGS = [
    "-Ctarget-feature=+crt-static"
  ];
  stripAllList = [ "bin" ];
  cargoLock = {
    lockFile = lib.cleanSource ./.. + "/Cargo.lock";
    allowBuiltinFetchGit = true;
  };
}

