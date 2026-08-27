{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

let
  inherit (import ./package.nix { inherit pkgs lib config; }) rustBuildDeps labeler;
in
{
  cachix.pull = [ "zeroecks-labeler" ];

  packages = with pkgs; [ bacon ] ++ rustBuildDeps;

  languages.rust.enable = true;

  enterTest = ''
    echo "Running tests"
    cargo test
  '';

  git-hooks.hooks = {
    shellcheck.enable = true;
    clippy.enable = true;
  };

  outputs = {
    inherit labeler;
    default = labeler;
  };

  env.SECRETSPEC_PROFILE = lib.mkIf (config.container.isBuilding) "default";
  env.SECRETSPEC_PROVIDER = lib.mkIf (config.container.isBuilding) "env";
  containers.labeler = {
    name = "labeler";
    maxLayers = 8;
    version = "nightly";
    startupCommand = "${labeler}/bin/labeler";
    entrypoint = [ "${labeler}/bin/labeler" ];
    copyToRoot = [
      (pkgs.buildEnv {
        name = "labeler-env";
        paths = [
          labeler
        ];
        pathsToLink = [ "/bin" ];
      })
      ./secretspec.toml
    ];

    fromImage = inputs.nix2container.packages.${pkgs.stdenv.system}.nix2container.pullImage {
      imageName = "docker.io/cloudron/base";
      imageDigest = "sha256:1c0666c9abe9e2090d33686826d4e97769b799124573118d41e0d7485135748e";
      sha256 = "sha256-mtlTI0S0t9nWZ68gKMl5ztLImN1bv6UGW9mV3YaAY/0=";
    };
  };

  scripts.release = {
    description = "Cut a new Cloudron release";
    packages = [
      pkgs.git
      pkgs.jq
      pkgs.nodejs
    ];
    exec = /* bash */ ''
      set -euo pipefail

      VERSION="''${1:?Usage: release <version> [publish-state]}"
      STATE="''${2:-published}"
      IMAGE="ghcr.io/zeroecks/labeler:''${VERSION}"
      ICON_URL="https://raw.githubusercontent.com/ZeroEcks/labeler/v''${VERSION}/assets/labeler.png"

      if [[ "$STATE" != "published" && "$STATE" != "testing" ]]; then
        echo "publish-state must be 'published' or 'testing', got '$STATE'" >&2
        exit 1
      fi

      cd "$DEVENV_ROOT"

      if [[ -n "$(git status --porcelain)" ]]; then
        echo "Working tree is dirty. Commit or stash changes before releasing." >&2
        exit 1
      fi

      if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
        echo "Tag v$VERSION already exists." >&2
        exit 1
      fi

      echo "==> [1/4] Bumping Cargo.toml and Cargo.lock to $VERSION"
      sed -i "0,/^version = \".*\"/s//version = \"$VERSION\"/" Cargo.toml
      sed -i "/^name = \"labeler\"\$/{n;s/^version = \".*\"/version = \"$VERSION\"/}" Cargo.lock

      echo "==> [2/4] Bumping CloudronManifest.json and prepending a CHANGELOG entry"
      jq --arg v "$VERSION" --arg icon "$ICON_URL" \
        '.version = $v | .upstreamVersion = $v | .iconUrl = $icon
         | .mediaLinks = (.mediaLinks | map(sub("/v[^/]+/"; "/v" + $v + "/")))' \
        CloudronManifest.json > CloudronManifest.json.tmp
      mv CloudronManifest.json.tmp CloudronManifest.json

      last_tag="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
      range="''${last_tag:+''${last_tag}..}HEAD"
      commits="$(git log "$range" --no-merges --pretty='format:* %s (%h)')"
      if [[ -z "$commits" ]]; then
        commits="* No commits recorded since ''${last_tag:-the initial commit}."
      fi

      {
        printf '[%s]\n%s\n\n' "$VERSION" "$commits"
        cat CHANGELOG
      } > CHANGELOG.tmp
      mv CHANGELOG.tmp CHANGELOG

      echo "==> [3/4] Registering v$VERSION in CloudronVersions.json"

      npx --yes "cloudron@''${CLOUDRON_CLI_VERSION:-latest}" versions add --image "$IMAGE" --state "$STATE"

      echo "==> [4/4] Committing and tagging v$VERSION"
      git add Cargo.toml Cargo.lock CloudronManifest.json CHANGELOG CloudronVersions.json
      git commit -m "chore: release v$VERSION"
      git tag -a "v$VERSION" -m "v$VERSION"

      echo "Pushing tags to github to trigger builds."
      sleep 5
      git push origin --follow-tags
      echo "Done!"
    '';
  };
}
