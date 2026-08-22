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

- **A virus scan on ingest.** Listed in M1, not implemented.
- **TLS and a reverse proxy.** `damd` speaks plain HTTP and expects something in front of it.
- **Human authentication.** The API authenticates bearer keys; there is no login, no session, and no SSO, so
  every person using the app needs a key minted for them with `damctl issue-key`.

## Probes and metrics

| Endpoint | Auth | Answers |
|---|---|---|
| `/health` | none | Liveness. Fixed body, discloses nothing — it is the first thing anybody scans. |
| `/ready` | none | Whether traffic should come here: Postgres and the object store, both checked, each named. `503` with a body saying which failed. |
| `/metrics` | bearer | Prometheus text. **404 when `server.metrics_token` is unset**, and 404 on a wrong token — a scan cannot tell "off" from "protected". |

`/ready` deliberately does not check the search index: a tenant's index opens lazily and is rebuildable, so
failing readiness over it would pull a replica out of rotation for something that stops no upload, download or
metadata write.

Metrics are `damrs_http_requests_total` and `damrs_http_request_duration_seconds` by method and **route
template**, plus `damrs_jobs` by kind and state. The route label is the template (`/assets/{asset_id}`) and
never the URI — a label per asset id is a million series and a monitoring outage. Status is a class (`2xx`,
`4xx`, `5xx`) rather than a code, because the questions asked of it are answered by the class.

`damrs_jobs{state="dead"}` is the one worth alerting on: a worker failing every derivative looks identical
from outside to one with nothing to do.

## Rate limiting

Off by default. `server.rate_limit_per_second` enables it on the **public** routes only — `/d/{token}`,
`/share/{token}`, `/portal/{key}` — keyed by client address, with `rate_limit_burst` (default 120) for the
burst, because a grid loads sixty thumbnails at once and a limiter tuned only on the sustained rate throttles
the first screen every user sees.

The authenticated API is deliberately **not** address-keyed. A company sits behind one or two egress
addresses, so that would be a limit on the customer as a whole — the whole art department sharing a bucket, and
one bulk upload starving everybody's thumbnails. Authenticated traffic is bounded by a revocable credential
and by the per-tenant quotas instead.

**Behind a proxy, set `server.trusted_proxy_hops`.** It defaults to zero, which trusts nothing but the socket.
A non-zero value counts entries from the *right* of `X-Forwarded-For`, because those are what proxies appended
and a client cannot forge them. Trusting the leftmost entry — the usual mistake — lets anybody claim a fresh
bucket per request, or exhaust somebody else's.

## Backups (§17, G11)

```sh
docker run --rm damrs:TAG damctl backup                      # every active tenant
docker run --rm damrs:TAG damctl backup --tenant acme        # one
docker run --rm damrs:TAG damctl restore-drill --tenant acme # prove one can be restored
docker run --rm damrs:TAG damctl dr-report                   # exits non-zero if any is unverified
```

`backup` takes a per-tenant logical dump with `pg_dump --format=custom` and uploads it to
`backups/<slug>/<timestamp>-<assets>.dump`, outside every tenant prefix so a lifecycle policy cannot tier a
backup into Glacier. Backups accumulate rather than overwrite: a corruption found on Thursday needs Tuesday's
copy.

`restore-drill` downloads the latest, replays it into a scratch schema, counts the assets against the number
recorded in the key, and drops the scratch schema — then writes `dr_state.last_verified_restore_at`. It is the
only thing that writes that column. A successful backup deliberately does not, because §17's whole argument is
that the gap between "we take backups" and "we have restored one" is where DR plans fail.

The live schema is renamed aside and back rather than restored over, so a drill cannot damage the thing it is
verifying — a dangerous drill is one nobody runs on the data that matters. Verified against a 185-asset
tenant: restored, counted, live schema intact, no leftover schemas.

`dr-report` exits non-zero while any tenant has never been verified, so it works as a check rather than only
as something to read.

**What this is not.** It is not point-in-time recovery. §17's five-minute RPO comes from WAL archiving to S3,
which is infrastructure — a managed Postgres, or `wal-g` alongside it — and an application that reimplemented
it would be a worse version of both. Nor does it back up the bucket: that is S3 versioning and cross-region
replication, which are bucket configuration. What is here is the per-tenant half, which is the half a physical
backup cannot do at all and the reason schema-per-tenant (D2) was chosen.

Run `backup` on a schedule and `restore-drill` on a slower one — weekly is a defensible starting point, since
the number it produces is the RTO you would publish.

## Volumes

`DAMRS_SEARCH__INDEX_ROOT` (default `/home/damrs/data/search`) holds the per-tenant Tantivy indexes. Give it a
volume: rebuilding from Postgres on every restart is a slow start and a cold cache. `damctl reindex --tenant
SLUG` exists for when an index is genuinely lost, which is the case a volume is protecting you from having to
handle during an incident.
