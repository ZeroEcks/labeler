{
  pkgs,
  lib,
  config,
}:

let
  inherit (lib) fileset;

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
    fileset = fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./secretspec.toml
      ./src
      ./templates
    ];
  };

  labeler = config.languages.rust.import labelerSrc { inherit crateOverrides; };
in
{
  inherit rustBuildDeps labeler;
}
