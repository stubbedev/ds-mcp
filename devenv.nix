{
  pkgs,
  ...
}:
{
  # cargo/rustc/rustfmt/clippy/rust-analyzer + RUST_SRC_PATH. Edition 2024,
  # all lints live in Cargo.toml [lints.clippy] — nothing extra to configure.
  languages.rust.enable = true;

  # Mirrors the repo flake's devShell extras: schema generation and the
  # -sys crates' build needs. Packaging stays on the flake (`just nix-build`).
  packages = with pkgs; [
    just
    pkg-config
    cmake
    perl
  ];
}
