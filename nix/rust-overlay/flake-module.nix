{ inputs, ... }:
let
  overlays = [
    (import inputs.rust-overlay)
    (
      _self: super:
      assert !(super ? rust-toolchain);
      {
        rust-toolchain = super.rust-bin.fromRustupToolchainFile ../../rust-toolchain.toml;
      }
    )
  ];
in
{
  perSystem = { system, pkgs, ... }: {
    _module.args.pkgs = import inputs.nixpkgs {
      inherit system overlays;
      config = { };
    };
    packages = {
      inherit (pkgs) rust-toolchain;

      rust-toolchain-versions = pkgs.writeScriptBin "rust-toolchain-versions" ''
        ${pkgs.rust-toolchain}/bin/cargo --version
        ${pkgs.rust-toolchain}/bin/rustc --version
      '';
    };
  };
}
