{
  description = "A tmux-like session manager with an fzf-style picker and no split screens";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.back-burner;
        back-burner = pkgs.rustPlatform.buildRustPackage {
          pname = "back-burner";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "A tmux-like session manager with an fzf-style picker and no split screens";
            mainProgram = "back-burner";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      overlays.default = final: _prev: {
        back-burner = self.packages.${final.stdenv.hostPlatform.system}.back-burner;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.back-burner ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
