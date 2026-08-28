{ inputs, lib, ... }:
{
  imports = [
    inputs.git-hooks-nix.flakeModule
  ];
  perSystem = { pkgs, self', ... }: {
    pre-commit.settings = {
      package = pkgs.prek;
      settings.rust.check.cargoDeps = pkgs.rustPlatform.importCargoLock {
        lockFile = ../../Cargo.lock;
      };
      hooks =
        let
          # Iterate all crate dependencies and get the buildInputs (such as openssl)
          labelerBuildInputs = lib.unique (
            builtins.concatMap (
              d: (d.buildInputs or [ ]) ++ (d.nativeBuildInputs or [ ])
            ) self'.packages.labeler.completeDeps
          );
        in
        {
          clippy = {
            enable = true;
            packageOverrides = {
              clippy = pkgs.rust-toolchain;
              cargo = pkgs.rust-toolchain;
            };
            settings.allFeatures = true;
            settings.denyWarnings = true;
            extraPackages = labelerBuildInputs;
          };
          rustfmt = {
            enable = true;
            packageOverrides = {
              rustfmt = pkgs.rust-toolchain;
              cargo = pkgs.rust-toolchain;
            };
          };
          statix.enable = true;
          deadnix.enable = true;
          actionlint.enable = true;
          zizmor.enable = true;
        };
    };
  };
}
