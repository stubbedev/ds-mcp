{
  perSystem =
    { buildTools, pkgs, ... }:
    {
      devShells.default = pkgs.mkShell {
        # Edition 2024; all lints live in Cargo.toml [lints.clippy], so the
        # toolchain needs nothing extra configured.
        packages =
          (with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            just
          ])
          ++ buildTools;
        # rust-analyzer resolves std from here.
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
      };

      formatter = pkgs.nixfmt;
    };
}
