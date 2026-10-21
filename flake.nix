{
  description = "DataStore MCP — multi-engine data-source MCP server";

  nixConfig = {
    # CI pushes every master build here; accept and skip the local rebuild.
    extra-substituters = [ "https://nix.stubbe.dev/c/default/default" ];
    extra-trusted-public-keys = [ "default:6uWvXutL9cXjV3lii+Ur5ff+ArQoG4kMBKNXWrIxhHg=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # crane builds the dependency crates as their own Cargo.lock-keyed
    # derivation, so a source edit reuses them instead of recompiling
    # libduckdb-sys' bundled C++ engine (~15 min of a ~25 min build).
    crane.url = "github:ipetkov/crane";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      imports = [
        ./nix/package.nix
        ./nix/shell.nix
        ./nix/toolchain.nix
      ];
    };
}
