# One image, three binaries, and a media toolchain — because the pipeline shells out.
#
# ## Why the runtime stage is not `scratch` or `distroless`
#
# `dam-media` does not link libvips or libavformat; it *executes* `vips`, `vipsheader`, `vipsthumbnail`,
# `ffmpeg` and `ffprobe` as subprocesses under a sandbox that clears the environment and applies rlimits
# (§18, `dam_media::sandbox`). A static Rust binary with no filesystem around it would start, serve the API,
# and then fail every derivative with "no `vips` on PATH" — which is the shape of failure that looks like a
# code bug for a day before anybody suspects the image.
#
# ## Why one image rather than three
#
# `damd`, `dam-worker` and `damctl` share the whole dependency tree and the media toolchain is by far the
# largest layer. Three images would triple the pull for the same bytes; one image with the command chosen at
# runtime keeps them in lockstep — and a worker running a different build from the API is a class of bug that
# is very hard to see, since both look healthy while the queue produces subtly different results.
#
# ## What is deliberately NOT here
#
# No migration on start. `damctl migrate --all` is a separate invocation of this same image, run once per
# deploy before the new code serves traffic — see `docker/DEPLOY.md`. A binary that migrates on boot means N
# replicas racing the same DDL, and the loser crash-loops.

# ─── builder ────────────────────────────────────────────────────────────────
# Pinned to the toolchain in .mise.toml. A different rustc is a different set of lints and, for edition 2024,
# potentially a different set of accepted programs.
FROM rust:1.94.0-bookworm AS builder

WORKDIR /src

# Dependencies first, so a source-only change does not re-download and re-build the tree. The manifests are
# copied without the sources, then a throwaway build populates the registry and target cache.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY bins/ bins/
COPY migrations/ migrations/

# No `.sqlx/` copy and no `SQLX_OFFLINE`, deliberately — and this is worth a note because two other places
# in the repo say otherwise. ARCHITECTURE §5.5 describes committed offline query metadata and .mise.toml
# exports `SQLX_OFFLINE=true` "so query! can resolve tenant tables". There are no `query!` macros anywhere in
# the tree: every statement is a runtime `sqlx::query()` over a string, so nothing needs a database at compile
# time and there is nothing for offline data to hold. A `COPY .sqlx/` here would simply fail the build.
# Release, and with the same debug info the profile asks for. A DAM that cannot symbolise a panic in a media
# pipeline is a DAM whose worst bugs are unreportable.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bins \
    && mkdir -p /out \
    && cp target/release/damd target/release/dam-worker target/release/damctl /out/

# ─── runtime ────────────────────────────────────────────────────────────────
FROM debian:trixie-slim AS runtime

# `libvips-tools` brings the three vips executables; `ffmpeg` brings both ffmpeg and ffprobe.
#
# **Trixie rather than bookworm, and the reason is a real failure rather than a preference.** Bookworm ships
# vips 8.14, which predates the rename of `vipsthumbnail --eprofile` to `--output-profile`. `dam-media` passes
# the new name whenever a render carries an output ICC profile (§18.1), so on 8.14 *every* vips-rendered
# derivative failed with `Unknown option --output-profile`. It was invisible for PNG, which the pure-Rust
# path handles, and total for HEIC, which only vips can decode — so an image built on bookworm serves a
# library where every iPhone photograph is stuck without a thumbnail.
#
# The divergence from development is still real and still worth stating: .mise.toml pins vips 8.18.5 and
# ffmpeg 9.0.1, because "a loader appearing or disappearing between vips builds changes which formats the
# probe claims to support". Trixie is closer but not equal. The build prints both versions and the loader
# list below so a base-image bump that moves either shows up in the build log, and `docker/DEPLOY.md` records
# pinning the toolchain exactly as the open item it is.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      libvips-tools \
      ffmpeg \
      postgresql-client \
 && rm -rf /var/lib/apt/lists/*

# `postgresql-client` is for backups: `damctl backup` and `damctl restore-drill` shell out to `pg_dump` and
# `pg_restore` (§17). Its major version must be at least the server's — pg_dump refuses a newer server — which
# is why the version is printed below rather than assumed.

# The formats the probe will claim, and the client versions the backups depend on. Printed at build time so a
# base-image bump that silently drops the HEIC loader, or moves pg_dump behind the server, shows up in the
# build log rather than in a user's failed upload or a failed restore.
RUN set -eu; \
    echo "vips:   $(vips --version)"; \
    echo "ffmpeg: $(ffmpeg -version | head -1)"; \
    echo "pg_dump: $(pg_dump --version)"; \
    for loader in jpegload pngload webpload heifload tiffload; do \
      if vips -l 2>/dev/null | grep -q "$loader"; then echo "  loader $loader: yes"; \
      else echo "  loader $loader: NO"; fi; \
    done

# Unprivileged, and with a real home: the sandbox writes temporary files, and a user with no writable
# directory fails on the first derivative rather than at start.
RUN useradd --create-home --shell /usr/sbin/nologin --uid 10001 damrs
WORKDIR /home/damrs

COPY --from=builder /out/damd /out/dam-worker /out/damctl /usr/local/bin/
# The tenant and global migrations, because `damctl migrate` reads them from disk at the path the binary was
# built with. Copied rather than embedded so an operator can diff what a deploy is about to apply.
COPY --chown=damrs:damrs migrations/ /home/damrs/migrations/

USER damrs

# Where the per-tenant Tantivy indexes live. A volume in any real deployment: an index rebuilt from Postgres
# on every restart is a slow start and a cold cache, and `damctl reindex` exists for when it is genuinely lost.
ENV DAMRS_SEARCH__INDEX_ROOT=/home/damrs/data/search
RUN mkdir -p /home/damrs/data/search

EXPOSE 8080

# No default `CMD` beyond the API, and the other two are chosen explicitly. A worker started by accident as
# an API replica would look healthy and process nothing.
CMD ["damd"]
