{
  perSystem =
    { pkgs, ... }:
    {
      # What the -sys crates' build scripts need, for both the package build
      # and the devShell.
      _module.args.buildTools = with pkgs; [
        pkg-config
        cmake # aws-lc-rs (russh crypto backend)
        perl
      ];
    };
}
