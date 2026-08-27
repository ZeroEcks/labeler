{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

let
  inherit (lib) fileset hasPrefix;
  rustBuildDeps = with pkgs; [
    pkg-config
    openssl.dev
    fontconfig
    freetype
    graphite2
    icu
    libpng
    zlib
  ];

  # Fixes for tectonic crates because of their native library usage
  renameLinksVar = correctLinks: attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        extra=$(sed -n 's/^export DEP_${lib.toUpper attrs.crateName}_/export DEP_${correctLinks}_/p' target/env)
        printf '%s\n' "$extra" >> target/env
      fi
    '';
  };
  relocateManifestSubdir = subdir: attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        cp -r "${subdir}" "$OUT_DIR/${subdir}"
        sed -i "s#$(pwd)/${subdir}#$lib/lib/${attrs.crateName}.out/${subdir}#g" target/env
      fi
    '';
  };
  fixupOutDirPath = attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        sed -i "s#$(pwd)/target/build/#$lib/lib/#g" target/env
      fi
    '';
  };
  mergePostConfigure = overrides: attrs: {
    postConfigure = lib.concatMapStringsSep "\n" (o: (o attrs).postConfigure or "") overrides;
  };

  crateOverrides = pkgs.defaultCrateOverrides // {
    tectonic_bridge_icu =
      attrs:
      {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.icu ];
        # icu needs the `.out` not `.dev`
        postInstall = ''
          if [ -f "$lib/lib/link" ]; then
            sed -i "s#${pkgs.icu.dev}#${pkgs.icu.out}#g" "$lib/lib/link"
          fi
        '';
      }
      // renameLinksVar "ICUUC" attrs;
    tectonic_bridge_graphite2 =
      attrs:
      {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.graphite2 ];
      }
      // renameLinksVar "GRAPHITE2" attrs;
    tectonic_bridge_png =
      attrs:
      {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [
          pkgs.libpng
          pkgs.zlib
        ];
      }
      // renameLinksVar "PNG" attrs;
    tectonic_bridge_freetype2 =
      attrs:
      {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.freetype ];
      }
      // renameLinksVar "FREETYPE2" attrs;
    tectonic_bridge_fontconfig =
      attrs:
      {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [
          pkgs.fontconfig
          pkgs.freetype
        ];
      }
      // renameLinksVar "FONTCONFIG" attrs;
    tectonic_bridge_harfbuzz = mergePostConfigure [
      (renameLinksVar "HARFBUZZ")
      fixupOutDirPath
    ];
    tectonic_xetex_layout = fixupOutDirPath;
    tectonic_bridge_core = relocateManifestSubdir "support";
    tectonic_bridge_flate = relocateManifestSubdir "include";
    tectonic_pdf_io = relocateManifestSubdir "pdf_io";
  };

  labelerSrc = fileset.toSource {
    root = ./.;
    fileset = fileset.intersection
      (fileset.fromSource (lib.sources.cleanSource ./.))
      (fileset.fileFilter (
        file:
        !(
          file.hasExt "json"
          || file.hasExt "md"
          || file.hasExt "sh"
          || file.hasExt "png"
          || hasPrefix "." file.name
          || hasPrefix "Cloudron" file.name
          || hasPrefix "devenv" file.name
          || hasPrefix "Docker" file.name
        )
      ) ./.);
  };
  
  labeler = config.languages.rust.import labelerSrc { inherit crateOverrides; };
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
    version = "nightly";
    startupCommand = "${labeler}/bin/labeler";
    entrypoint = [ "${labeler}/bin/labeler" ];
    copyToRoot = pkgs.buildEnv {
      name = "labeler-env";
      paths = [ labeler ];
      pathsToLink = [ "/bin" ];
    };
    fromImage = inputs.nix2container.packages.${pkgs.stdenv.system}.nix2container.pullImage {
      imageName = "docker.io/cloudron/base";
      imageDigest = "sha256:1c0666c9abe9e2090d33686826d4e97769b799124573118d41e0d7485135748e";
      sha256 = "sha256-mtlTI0S0t9nWZ68gKMl5ztLImN1bv6UGW9mV3YaAY/0=";
    };
  };
}
