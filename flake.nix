{
  description = "npd - an MPD-compatible daemon backed by Navidrome / OpenSubsonic";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };
      homeModules.default = import ./nix/hm-module.nix { inherit self; };
      # Older home-manager setups still read homeManagerModules.
      homeManagerModules.default = self.homeModules.default;

      overlays.default = final: prev: {
        npd = final.callPackage ./nix/npd.nix { };
      };

      packages = forAllSystems (pkgs: rec {
        npd = pkgs.callPackage ./nix/npd.nix { };
        default = npd;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.npd ];

          packages = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            # Handy for poking at the daemon while it runs.
            mpc
            sqlite
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
