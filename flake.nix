{
  description = "DataStore MCP — multi-engine data-source MCP server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "ds-mcp";
          inherit version;
          src = ./.;
          # Cargo.lock is exact by construction — no vendorHash dance needed.
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake # aws-lc-rs (russh crypto backend)
            perl
          ];
          # The ssh tunnel test needs docker; everything else runs.
          checkFlags = [ "--skip=ssh_tunnel" ];
          meta = {
            description = "Multi-engine data-source MCP server";
            license = pkgs.lib.licenses.mit;
            mainProgram = "ds-mcp";
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            just
            pkg-config
            cmake
            perl
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        default = self.packages.${pkgs.system}.default;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
