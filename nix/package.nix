{ inputs, ... }:
{
  perSystem =
    {
      buildTools,
      lib,
      pkgs,
      self',
      ...
    }:
    let
      craneLib = inputs.crane.mkLib pkgs;
      version = (lib.importTOML ../Cargo.toml).package.version;

      # Only what the build reads. Editing the README, the workflows, the
      # packaging scripts or the nix/ modules then costs nothing.
      src = lib.fileset.toSource {
        root = ../.;
        fileset = lib.fileset.unions [
          ../Cargo.toml
          ../Cargo.lock
          # Sets CXXSTDLIB_* so the cc crate does not add its own dynamic
          # -lstdc++ for duckdb's bundled C++ engine; build.rs links it
          # statically instead. Dropping it silently changes how the binary
          # links.
          ../.cargo
          ../build.rs
          ../src
          ../tests
        ];
      };

      # Shared by the deps layer and the final build. crane keys the deps
      # derivation on Cargo.lock plus these inputs — not on our source — so
      # editing src/ reuses them.
      commonArgs = {
        inherit src;
        pname = "ds-mcp";
        strictDeps = true;
        nativeBuildInputs = buildTools;
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
      packages = {
        default = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts version;
            # The `rust` CI job runs the whole suite on this commit in under
            # two minutes. Running it again here costs a second full compile
            # of ds-mcp against the test harness (~3.5 min in CI) to learn
            # the same thing, so this build only produces the binary.
            doCheck = false;
            meta = {
              description = "Multi-engine data-source MCP server";
              license = lib.licenses.mit;
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
      };

      checks.default = self'.packages.default;
    };
}
