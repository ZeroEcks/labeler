_: {
  projectRootFile = "flake.nix";
  programs = {
    nixfmt.enable = true;
    deadnix.enable = true;
    statix.enable = true;
    rustfmt.enable = true;
    shellcheck.enable = true;
    zizmor.enable = true;
    actionlint.enable = true;
  };
}
