{
  description = "OCI container runtime monitor";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];

      overlay = import ./nix/overlay.nix;

      mkPkgs =
        system: args:
        import nixpkgs (
          {
            inherit system;
            overlays = [ overlay ];
          }
          // args
        );

      mkConmonrs = pkgs: pkgs.callPackage ./nix/derivation.nix { };
    in
    {
      packages = forAllSystems (system: {
        default = mkConmonrs (mkPkgs system { });

        amd64 = mkConmonrs (mkPkgs system { });

        arm64 = mkConmonrs (mkPkgs system {
          crossSystem.config = "aarch64-unknown-linux-gnu";
        });

        ppc64le = mkConmonrs (mkPkgs system {
          crossSystem.config = "powerpc64le-unknown-linux-gnu";
        });

        s390x = mkConmonrs (mkPkgs system {
          crossSystem.config = "s390x-unknown-linux-musl";
        });
      });
    };
}
