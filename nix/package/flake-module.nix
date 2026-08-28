{ inputs, lib, ... }:
let
  inherit (lib) fileset;
in
{
  perSystem =
    { system, pkgs, ... }:
    let
      labelerSrc = fileset.toSource {
        root = ./../..; # Repository root
        fileset = fileset.unions [
          ../../Cargo.toml
          ../../Cargo.lock
          ../../secretspec.toml
          ../../src
          ../../templates
        ];
      };

      buildRustCrateForPkgs =
        pkgs:
        pkgs.buildRustCrate.override {
          rustc = pkgs.rust-toolchain;
          cargo = pkgs.rust-toolchain;
        };

      generatedCargoNix = inputs.crate2nix.tools.${system}.generatedCargoNix {
        name = "rustnix";
        src = labelerSrc;
      };

      cargoNix = import generatedCargoNix { inherit pkgs buildRustCrateForPkgs; };
    in
    {
      checks = {
        labeler = cargoNix.rootCrate.build.override {
          runTests = true;
        };
      };
      packages = rec {
        labeler = cargoNix.rootCrate.build;
        default = labeler;
      };
    };
}
