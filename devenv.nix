{ pkgs, lib, config, inputs, ... }:

let
  rustBuildDeps = with pkgs; [
    pkg-config
    openssl.dev
    fontconfig freetype graphite2 icu libpng zlib
  ];

  # crate2nix's `buildRustCrate` builds each crate in its own sandboxed
  # derivation, so these bridge crates don't see the `pkg-config`/native libs
  # from `rustBuildDeps` above (that list only wires up the interactive
  # devShell). `tectonic`'s bridge_{icu,graphite2,png,freetype2,fontconfig}
  # crates always probe their system library via pkg-config (unlike
  # bridge_harfbuzz, which vendors its own copy by default), so each needs
  # `pkg-config` plus the matching library wired in explicitly.
  # https://nix-community.github.io/crate2nix/30_building/30_crateoverrides.html
  #
  # Separately, `buildRustCrate` derives the `DEP_<...>_` env var it exports
  # for downstream build scripts from the crate *name* (e.g. `DEP_TECTONIC_
  # BRIDGE_GRAPHITE2_INCLUDE_PATH`), whereas real Cargo derives it from the
  # `links` manifest key (e.g. `DEP_GRAPHITE2_INCLUDE_PATH`, which is what
  # tectonic's own build scripts actually read). Where those differ, append a
  # correctly-named copy of every `target/env` line so dependents relying on
  # the real `links` name (harfbuzz -> graphite2, engine_xetex -> harfbuzz/
  # freetype2/icuuc/fontconfig, pdf_io -> png) can find it.
  renameLinksVar = correctLinks: attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        extra=$(sed -n 's/^export DEP_${lib.toUpper attrs.crateName}_/export DEP_${correctLinks}_/p' target/env)
        printf '%s\n' "$extra" >> target/env
      fi
    '';
  };

  # `tectonic_bridge_core`, `tectonic_bridge_flate`, and `tectonic_pdf_io`
  # export their C header locations as `cargo:include(-path)=$CARGO_MANIFEST_
  # DIR/<subdir>` — a path inside their own ephemeral build sandbox. Real
  # Cargo gets away with this because a crate's source stays on disk (in the
  # registry cache) for the lifetime of the whole `cargo build`; under
  # crate2nix each crate is its own derivation, so that directory is gone by
  # the time a dependent (pdf_io, engine_xetex, engine_xdvipdfmx) tries to
  # read it. Copy just the referenced subdirectory into `$OUT_DIR` (which
  # `buildRustCrate` already persists to `$lib/lib/<crateName>.out`) and
  # rewrite `target/env` to point there instead, so the path survives into
  # dependents' builds.
  relocateManifestSubdir = subdir: attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        cp -r "${subdir}" "$OUT_DIR/${subdir}"
        sed -i "s#$(pwd)/${subdir}#$lib/lib/${attrs.crateName}.out/${subdir}#g" target/env
      fi
    '';
  };

  # `tectonic_bridge_harfbuzz` (vendored, non-`external-harfbuzz` build) and
  # `tectonic_xetex_layout` copy their headers into `$OUT_DIR` and export
  # `cargo:include-path=$OUT_DIR[;...]` — better than the raw-source-dir case
  # above, but `$OUT_DIR` is still the *ephemeral* build-sandbox path at the
  # point the value is written into `target/env`, even though its contents
  # get persisted to `$lib/lib/<crateName>.out` by `buildRustCrate` itself.
  # Rewrite the recorded value to match where the contents actually end up.
  fixupOutDirPath = attrs: {
    postConfigure = ''
      if [ -f target/env ]; then
        sed -i "s#$(pwd)/target/build/#$lib/lib/#g" target/env
      fi
    '';
  };
  # Compose several of the above per-crate postConfigure fixups. Order
  # matters: later fixups' `sed`s also see (and can further rewrite) lines
  # appended by earlier ones.
  mergePostConfigure = overrides: attrs: {
    postConfigure = lib.concatMapStringsSep "\n" (o: (o attrs).postConfigure or "") overrides;
  };


  crateOverrides = pkgs.defaultCrateOverrides // {
    tectonic_bridge_icu = attrs: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.icu ];
      # nixpkgs' icu4c ships a `.pc` file whose `libdir` incorrectly points
      # at the `dev` output, which has no `.so` files (they live in `out`).
      # `tectonic_dep_support`'s pkg-config probe bakes that wrong `-L` path
      # into this crate's exported link flags, breaking the final binary
      # link with "cannot find -licuuc". Rewrite it once installed.
      postInstall = ''
        if [ -f "$lib/lib/link" ]; then
          sed -i "s#${pkgs.icu.dev}#${pkgs.icu.out}#g" "$lib/lib/link"
        fi
      '';
    } // renameLinksVar "ICUUC" attrs;
    tectonic_bridge_graphite2 = attrs: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.graphite2 ];
    } // renameLinksVar "GRAPHITE2" attrs;
    tectonic_bridge_png = attrs: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.libpng pkgs.zlib ];
    } // renameLinksVar "PNG" attrs;
    tectonic_bridge_freetype2 = attrs: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.freetype ];
    } // renameLinksVar "FREETYPE2" attrs;
    tectonic_bridge_fontconfig = attrs: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.fontconfig pkgs.freetype ];
    } // renameLinksVar "FONTCONFIG" attrs;
    tectonic_bridge_harfbuzz = mergePostConfigure [ (renameLinksVar "HARFBUZZ") fixupOutDirPath ];
    tectonic_xetex_layout = fixupOutDirPath;
    tectonic_bridge_core = relocateManifestSubdir "support";
    tectonic_bridge_flate = relocateManifestSubdir "include";
    tectonic_pdf_io = relocateManifestSubdir "pdf_io";
  };


  labeler = config.languages.rust.import (lib.cleanSource ./.) { inherit crateOverrides; };
in
{
  # https://devenv.sh/packages/
  packages = with pkgs; [ bacon ] ++ rustBuildDeps;

  # https://devenv.sh/languages/
  # languages.rust.enable = true;
  languages.rust.enable = true;

  # Secrets
  env.STRIPE_SECRET_KEY = config.secretspec.secrets.STRIPE_SECRET_KEY;

  # https://devenv.sh/processes/
  processes.dev.exec = "${lib.getExe pkgs.bacon}";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/basics/
  enterShell = ''
  '';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    cargo clippy
    cargo test
  '';

  # https://devenv.sh/git-hooks/
  git-hooks.hooks= {
    shellcheck.enable = true;
    clippy.enable = true;
  };

  outputs = {
    inherit labeler;
    default = labeler;
  };

  # https://devenv.sh/containers/
  # Packages the compiled `labeler` binary into a minimal OCI image. This
  # image is never run directly: CI builds it, then the Cloudron Dockerfile
  # copies the binary and its Nix runtime closure out of it via a
  # multi-stage `COPY --from=`. The Cloudron base image supplies the actual
  # runtime (addons, healthchecks, syslog).
  containers.builder = {
    name = "labeler-builder";
    copyToRoot = pkgs.buildEnv {
      name = "labeler-root";
      paths = [ labeler ];
      pathsToLink = [ "/bin" ];
    };
    startupCommand = "${labeler}/bin/labeler";
    # Skip the default devenv-shell entrypoint: it pulls in the full Rust
    # toolchain closure, which this image doesn't need.
    entrypoint = [ "${labeler}/bin/labeler" ];
  };
}
