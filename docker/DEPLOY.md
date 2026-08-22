# Deploying damrs

One image, three commands, and a migration step that runs before any of them serve traffic.

```sh
docker build -t damrs:$(git rev-parse --short HEAD) .
```

The image carries `damd`, `dam-worker`, `damctl` and the media toolchain the pipeline shells out to. It does
**not** carry the frontend: `web/` is a SvelteKit app built and served separately (or statically hosted), and
it reaches the API over the URL in `server.public_url`.

## Order of operations for a deploy

1. **Migrate, once, before anything new serves.** `docker run --rm damrs:TAG damctl migrate --all`.
2. **Roll the API.** `damd`, the default command.
3. **Roll the workers.** `dam-worker`, at least one. Nothing appears in a grid without it: an upload lands in
   staging and stops there, and no asset gets a thumbnail.

Migration is a separate invocation rather than something a binary does at boot, because N replicas starting
together would race the same DDL and the losers crash-loop. It is also the step you want to be able to run,
read the output of, and stop before rolling anything.

## Configuration

Everything is `DAMRS_`-prefixed with `__` as the section separator, and unknown keys are a **startup
failure** rather than a warning — `DAMRS_S3_BUCKET` is not a loose alias for `storage.bucket`, it is a refusal
to start. `docker run --rm damrs:TAG damctl config` prints the resolved configuration with secrets redacted,
which is the fastest way to find out what a deployment actually thinks it is doing.

The three that have no safe default:

| Key | Why |
|---|---|
| `DAMRS_DATABASE__URL` | Defaults to a localhost dev database. |
| `DAMRS_SERVER__URL_SIGNING_KEY` | Defaults to a **public placeholder**, and `validate()` refuses to start in production with it — every signed URL would be forgeable by anyone who has read the source. |
| `DAMRS_STORAGE__BUCKET` | Defaults to `damrs-dev`. |

`storage.endpoint` is empty by default, which means real AWS S3 and the ambient credential chain — instance
role, IRSA, or environment. Set it only for MinIO, Ceph, SeaweedFS or another gateway, in which case
`storage.access_key_id` and `storage.secret_access_key` become required.

## Two things that will bite, both found by doing this

**The signing endpoint is the client's endpoint.** A delivery URL is a `302` to a presigned S3 URL, and the
signature covers the `Host` header — so the endpoint the server signs with must be the one the browser
connects to. Rewriting the hostname in the redirect invalidates it with a `403` that says nothing about why.
There is currently no way to configure an internal connect endpoint and a separate public signing endpoint;
if your object storage is reachable under two names, they have to be the same name here. Verified the hard
way: signing against `host.docker.internal` and fetching via `localhost` returns `403 SignatureDoesNotMatch`.

**The vips version matters, and not only for formats.** `vipsthumbnail`'s ICC flag is `--output-profile` from
8.18 and `--export-profile` before it. The toolchain now asks the binary its version at discovery and picks
accordingly, so both work — but this was a total failure of every vips-rendered derivative on a Debian trixie
image before that fix, and it was invisible for PNG and JPEG because the pure-Rust path decodes those. Only
HEIC exposed it. If you change the base image, re-render one HEIC before believing anything.

## The toolchain divergence, which is still open

`.mise.toml` pins vips 8.18.5 and ffmpeg 9.0.1 for development. The image takes whatever the base
distribution ships — currently vips 8.16.1 and ffmpeg 7.1.5 on `debian:trixie-slim`. The pin exists because
"a loader appearing or disappearing between vips builds changes which formats the probe claims to support",
and that argument applies to the image at least as strongly.

The build prints both versions and checks for the loaders that matter, so a base-image bump that drops HEIC
support shows up in the build log rather than in a user's failed upload. That is a tripwire, not a fix. The fix
is to pin the toolchain in the image the way development pins it — a vips build stage, or a base image that
carries a known version — and it is not done.

## What a real deployment needs that this does not provide

Named honestly, because an image is not a deployment:

- **Backups and restore drills** (G11). Nothing here backs up Postgres or the bucket.
- **Metrics and a readiness probe.** `/health` exists. There is no `/metrics`, no `/ready`, and nothing
  scrapes the OTLP exporter `dam-telemetry` can configure.
- **Rate limiting.** `governor` is a declared dependency of `dam-api` and is never called, including on the
  public delivery routes.
- **A virus scan on ingest.** Listed in M1, not implemented.
- **TLS and a reverse proxy.** `damd` speaks plain HTTP and expects something in front of it.
- **Human authentication.** The API authenticates bearer keys; there is no login, no session, and no SSO, so
  every person using the app needs a key minted for them with `damctl issue-key`.

## Volumes

`DAMRS_SEARCH__INDEX_ROOT` (default `/home/damrs/data/search`) holds the per-tenant Tantivy indexes. Give it a
volume: rebuilding from Postgres on every restart is a slow start and a cold cache. `damctl reindex --tenant
SLUG` exists for when an index is genuinely lost, which is the case a volume is protecting you from having to
handle during an incident.
