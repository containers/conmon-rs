{
  description = "OCI container runtime monitor";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
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

      mkConmonrs =
        pkgs:
        pkgs.callPackage ./nix/derivation.nix {
          rev = self.rev or self.dirtyRev or "";
          lastModified = toString (self.lastModified or 0);
        };

      # Builds natively if the target is the system itself, including its
      # libc, cross otherwise.
      mkTarget =
        system: config:
        mkConmonrs (
          mkPkgs system (
            nixpkgs.lib.optionalAttrs
              ((nixpkgs.lib.systems.elaborate config).config != (nixpkgs.lib.systems.elaborate system).config)
              {
                crossSystem.config = config;
              }
          )
        );
    in
    {
      packages = forAllSystems (system: {
        default = mkConmonrs (mkPkgs system { });

        amd64 = mkTarget system "x86_64-unknown-linux-gnu";

        arm64 = mkTarget system "aarch64-unknown-linux-gnu";

        ppc64le = mkTarget system "powerpc64le-unknown-linux-gnu";

        s390x = mkTarget system "s390x-unknown-linux-musl";
      });
    };
}
