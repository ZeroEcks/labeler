{
  description = "Labler Application";

  inputs = {
    git-hooks-nix.url = "github:cachix/git-hooks.nix";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crate2nix = {
      url = "github:nix-community/crate2nix";
      inputs = {
        flake-parts.follows = "flake-parts";
        nixpkgs.follows = "nixpkgs";
        cachix.inputs.nixpkgs.follows = "nixpkgs";
      };
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nix2container.url = "github:nlewo/nix2container";
  };

  nixConfig = {
    # crate2nix cache
    extra-trusted-public-keys = "eigenvalue.cachix.org-1:ykerQDDa55PGxU25CETy9wF6uVDpadGGXYrFNJA3TUs= zeroecks-labeler.cachix.org-1:HuxIsRY38Fby007U8jzMgyF/UG2UAafOLMrj/c1ivD8=";
    extra-substituters = "https://eigenvalue.cachix.org https://zeroecks-labeler.cachix.org";
    allow-import-from-derivation = true;
  };

  outputs =
    inputs@{
      flake-parts,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      imports = [
        # Rust toolchain
        ./nix/rust-overlay/flake-module.nix
        # Actual rust binary
        ./nix/package/flake-module.nix
        # Formatter
        ./nix/treefmt/flake-module.nix
        ./nix/git-hooks/flake-module.nix
        ./nix/devshell/flake-module.nix
        # Cloudron container and release (TODO: Split the CI + cloudron portions)
        ./nix/cloudron/flake-module.nix
      ];

      perSystem = _: {
      };
    };
}
