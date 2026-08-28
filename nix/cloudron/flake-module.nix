{ inputs, self, ... }:
{
  imports = [
    inputs.treefmt-nix.flakeModule
  ];
  perSystem =
    {
      inputs',
      pkgs,
      self',
      ...
    }:
    let
      nix2containerPkgs = inputs'.nix2container.packages;
      labeler = self'.packages.labeler;

      cloudronStartScript = pkgs.writeShellApplication {
        name = "start.sh";
        runtimeInputs = [ labeler ];
        text = ''
          exec labeler
        '';
      };

      cloudronBaseImage = nix2containerPkgs.nix2container.pullImage {
        imageName = "docker.io/cloudron/base";
        imageDigest = "sha256:1c0666c9abe9e2090d33686826d4e97769b799124573118d41e0d7485135748e"; # v5.1.0
        sha256 = "sha256-mtlTI0S0t9nWZ68gKMl5ztLImN1bv6UGW9mV3YaAY/0=";
      };

      tagImageFromGitTag = pkgs.writeShellApplication {
        name = "tag-image-from-git-tag";
        runtimeInputs = [
          pkgs.crane
          pkgs.git
        ];
        text =
          let
            rev = self.rev or "dirty";
          in
          /* bash */ ''
            REV="${rev}"
            if [ "''${REV}" == 'dirty' ]; then
              echo "Error: Was built from a dirty revision."
              exit 1
            fi

            if git describe --tags --exact-match --abbrev=0 "${rev}" >/dev/null 2>&1; then
              CURRENT_TAG=$(git describe --tags --exact-match --abbrev=0 "${rev}")
            else
              echo "Error: The current revision ${rev} is not tagged. Please tag the commit before running this script."
              exit 1
            fi

            if [[ ! "$CURRENT_TAG" =~ ^v[0-9] ]]; then
              echo "Error: Tag '$CURRENT_TAG' does not match required format v<semver> (e.g. v1.2.3)."
              exit 1
            fi

            # Strip the leading 'v' so the image tag is a bare semver
            IMAGE_TAG=''${CURRENT_TAG#v}

            crane tag ghcr.io/zeroecks/labeler:${rev} "$IMAGE_TAG"
          '';
      };
      createRelease = pkgs.writeShellApplication {
        name = "create-release";
        runtimeInputs = [
          pkgs.git
          pkgs.jq
          pkgs.nodejs
        ];
        text = /* bash */ ''
          set -euo pipefail

          VERSION="''${1:?Usage: create-release <version> [publish-state]}"
          STATE="''${2:-published}"
          IMAGE="ghcr.io/zeroecks/labeler:''${VERSION}"
          ICON_URL="https://raw.githubusercontent.com/ZeroEcks/labeler/v''${VERSION}/assets/labeler.png"

          if [[ "$STATE" != "published" && "$STATE" != "testing" ]]; then
            echo "publish-state must be 'published' or 'testing', got '$STATE'" >&2
            exit 1
          fi

          cd "$(git rev-parse --show-toplevel)"

          current_branch="$(git rev-parse --abbrev-ref HEAD)"
          if [[ "$current_branch" != "main" ]]; then
            echo "Must be on main to cut a release (currently on $current_branch)." >&2
            exit 1
          fi

          if [[ -n "$(git status --porcelain)" ]]; then
            echo "Working tree is dirty. Commit or stash changes before releasing." >&2
            exit 1
          fi

          if git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
            echo "Tag v$VERSION already exists." >&2
            exit 1
          fi

          echo "==> [1/6] Bumping Cargo.toml, Cargo.lock and CloudronManifest.json to $VERSION"
          sed -i "0,/^version = \".*\"/s//version = \"$VERSION\"/" Cargo.toml
          sed -i "/^name = \"labeler\"\$/{n;s/^version = \".*\"/version = \"$VERSION\"/}" Cargo.lock

          jq --arg v "$VERSION" --arg icon "$ICON_URL" \
            '.version = $v | .upstreamVersion = $v | .iconUrl = $icon
             | .mediaLinks = (.mediaLinks | map(sub("/v[^/]+/"; "/v" + $v + "/")))' \
            CloudronManifest.json > CloudronManifest.json.tmp
          mv CloudronManifest.json.tmp CloudronManifest.json

          echo "==> [2/6] Updating CHANGELOG"
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

          echo "==> [3/6] Registering v$VERSION in CloudronVersions.json"
          npx --yes "cloudron@''${CLOUDRON_CLI_VERSION:-latest}" versions add --image "$IMAGE" --state "$STATE"

          commit_msg="chore: release v''${VERSION}

          ''${commits}"

          echo "==> [4/6] Committing v$VERSION"
          git add Cargo.toml Cargo.lock CloudronManifest.json CHANGELOG CloudronVersions.json
          git commit -m "$commit_msg"

          echo "==> [5/6] Tagging v$VERSION"
          git tag -a "v$VERSION" -m "$commit_msg"

          echo "==> [6/6] Pushing main and the new tag"
          git push origin main --follow-tags

          echo "Done!"
        '';
      };

      buildImagePushImageTagRelease = pkgs.writeShellApplication {
        name = "build-image-push-image-tag-release";
        runtimeInputs = [
          self'.packages.cloudron.copyToRegistry
          tagImageFromGitTag
        ];
        text = ''
          # Image is already built by the time this script runs
          # Copy from nix into the ghcr.io registry
          copy-to-registry
          # Use crane to add a tag to the revision we just uploaded to the tag
          tag-image-from-git-tag || true # if it errors it means we didn't want to push the tag anyway
        '';
      };
    in
    {
      packages = {
        inherit cloudronStartScript createRelease;
        ci = pkgs.symlinkJoin {
          name = "CI tools";
          paths = [
            buildImagePushImageTagRelease
            tagImageFromGitTag
          ];
        };

        cloudron = nix2containerPkgs.nix2container.buildImage {
          name = "ghcr.io/zeroecks/labeler";
          # use the git sha or dirty if we are not on a clean commit
          tag = if (self ? rev) then self.rev else "dirty";
          config = {
            Cmd = [ "/app/pkg/labeler" ];
            WorkingDir = "/app/pkg";
            Env = [
              "SECRETSPEC_PROVIDER=env"
            ];
          };
          copyToRoot = [
            # Assemble the filesystem
            (pkgs.symlinkJoin {
              name = "cloudron-container-layout";

              paths = [
                "${labeler}/bin"
                "${cloudronStartScript}/bin"
              ];

              postBuild = ''
                # create our working directory in the container
                mkdir -p $out/app/pkg/

                # Copy the secretspec file needed at runtime to the working directory
                cp ${../../secretspec.toml} $out/app/pkg/secretspec.toml

                # Copy all files from paths above to /app/pkg/
                for item in $out/*; do
                  if [ "$item" != "$out/app" ]; then
                    mv "$item" $out/app/pkg/
                  fi
                done
              '';
            })
          ];

          fromImage = cloudronBaseImage;
        };
      };

    };
}
