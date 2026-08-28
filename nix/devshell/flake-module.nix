_: {
  perSystem = { pkgs, config, ... }: {
    devShells.default =
      with pkgs;
      mkShell {
        shellHook = ''
          ${config.pre-commit.shellHook}
          echo 1>&2 "Activated 'labeler' development environment"
        '';
        buildInputs = [
          openssl
          pkg-config
          rust-toolchain # comes from rust-overlay
        ]
        ++ config.pre-commit.settings.enabledPackages;
      };
  };
}
