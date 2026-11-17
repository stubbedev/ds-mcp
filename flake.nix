{
  description = "DataStore MCP — multi-engine data-source MCP server";

  nixConfig = {
    # CI pushes every master build here; accept and skip the local rebuild.
    extra-substituters = [ "https://xilos.dev/c/stubbedev/main" ];
    extra-trusted-public-keys = [ "xilos.dev/stubbedev/main-1:397R6S49v/FSvP+mZ3HlKrVN1KXhgzHIEIHxA/UuG6Y=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # crane builds the dependency crates as their own Cargo.lock-keyed
    # derivation, so a source edit reuses them instead of recompiling them.
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
