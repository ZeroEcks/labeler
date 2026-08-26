# The `labeler` binary and its Nix runtime closure (glibc, openssl,
# fontconfig, icu, ...) are built separately via `devenv container build
# builder` (see devenv.nix) and published by CI/the release script as
# `<owner>/<repo>-builder:<tag>`. This build stage only re-exposes that
# already-built image so the final stage can copy the binary out of it;
# nothing is compiled here.
ARG BUILDER_IMAGE=ghcr.io/zeroecks/labeler-builder:latest
FROM ${BUILDER_IMAGE} AS builder

FROM cloudron/base:5.1.0@sha256:1c0666c9abe9e2090d33686826d4e97769b799124573118d41e0d7485135748e

RUN mkdir -p /app/code
WORKDIR /app/code

# labeler is dynamically linked against libraries in the Nix store; copy the
# whole closure alongside the binary so the linker can find them.
COPY --from=builder /nix/store /nix/store
COPY --from=builder /env/bin/labeler /app/labeler
COPY secretspec.toml /app/code/secretspec.toml

COPY start.sh /app/pkg/

CMD [ "/app/pkg/start.sh" ]
