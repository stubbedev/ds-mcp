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
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
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
      packages = forAllSystems (
        pkgs:
        let
          craneLib = crane.mkLib pkgs;

          # Only what the build reads. Editing the README, the workflows or the
          # packaging scripts then costs nothing.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              # Sets CXXSTDLIB_* so the cc crate does not add its own dynamic
              # -lstdc++ for duckdb's bundled C++ engine; build.rs links it
              # statically instead. Dropping it silently changes how the binary
              # links.
              ./.cargo
              ./build.rs
              ./src
              ./tests
            ];
          };

          # Shared by the deps layer and the final build. crane keys the deps
          # derivation on Cargo.lock plus these inputs — not on our source — so
          # editing src/ reuses them.
          commonArgs = {
            inherit src;
            pname = "ds-mcp";
            strictDeps = true;
            nativeBuildInputs = with pkgs; [
              pkg-config
              cmake # aws-lc-rs (russh crypto backend)
              perl
            ];
          };

          # crane folds Cargo.toml and the ds-mcp entry of Cargo.lock into the
          # deps layer's dummy source, and both carry the version. Every
          # `chore: bump to vX.Y.Z` would therefore recompile every dependency —
          # including duckdb's bundled C++ engine — on the one commit that
          # changes no dependency at all. Pin the version to 0.0.0 here so only
          # a real Cargo.lock change invalidates the layer; the final build
          # below still gets the true version.
          depsSrc = pkgs.runCommandLocal "ds-mcp-deps-src" { } ''
            cp -r --no-preserve=mode,ownership ${src} $out
            chmod -R u+w $out
            sed -i 's/^version = "${version}"$/version = "0.0.0"/' $out/Cargo.toml
            sed -i '/^name = "ds-mcp"$/{n;s/^version = "${version}"$/version = "0.0.0"/;}' \
              $out/Cargo.lock
          '';

          # The cached dependency layer. Tests belong to the final build (and to
          # CI's own test job), so skip them here: it keeps the artifact that
          # goes to the binary cache smaller and the layer faster.
          cargoArtifacts = craneLib.buildDepsOnly (
            commonArgs
            // {
              src = depsSrc;
              version = "0.0.0";
              doCheck = false;
            }
          );
        in
        {
          default = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts version;
              # The ssh tunnel test needs docker; everything else runs.
              cargoTestExtraArgs = "-- --skip=ssh_tunnel";
              meta = {
                description = "Multi-engine data-source MCP server";
                license = pkgs.lib.licenses.mit;
                mainProgram = "ds-mcp";
              };
            }
          );

          # Not useful to install. It exists so CI can push the compiled
          # dependency layer to the binary cache as its own store path: deps are
          # a build-time input, absent from the binary's runtime closure, so
          # pushing `default` alone never carries them — and an unpushed layer
          # is a ~25 min recompile for every consumer.
          deps = cargoArtifacts;
        }
      );

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
