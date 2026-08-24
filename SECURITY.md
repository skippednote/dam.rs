# Security

## Reporting a vulnerability

Use **[GitHub's private vulnerability reporting](https://github.com/skippednote/dam.rs/security/advisories/new)**
on this repository. It opens a private thread with the maintainers and needs no email address from either side.

Please do not open a public issue for a security problem. There is no bug bounty; there is a maintainer who
would rather hear about it than read about it.

Include what you have — a request that demonstrates it is worth more than a description of what it might do.
You will get an acknowledgement, and if the report is valid, a note when the fix lands and credit in the
advisory unless you would rather not have it.

## What is in scope

dam.rs is a multi-tenant system holding other people's media and the rights information that decides who may
have it. The boundaries that matter most, and where a report is most useful:

- **Tenant isolation.** Every tenant is a PostgreSQL schema, reached only through `TenantConn`, which cannot
  be constructed outside a transaction. Anything that reads or writes across that boundary is the most serious
  class of bug this project has.
- **Delivery.** Every download, render and connector fetch goes through one signed URL, and rights are
  evaluated *at delivery* rather than at signing — so a valid signature is permission to attempt, not
  permission to receive. A path that returns bytes without that evaluation is in scope even if the signature
  checks out.
- **Access scoping.** Access is compiled once into a predicate that travels with the caller. A query that
  reaches assets without rendering it — including a count, since §7 treats a count as a disclosure — is in
  scope.
- **Credentials.** API keys, connector signing secrets, SCIM provisioning tokens and share tokens are stored
  hashed or sealed and shown once. A path that reads one back, logs one, or accepts a forged one is in scope.
- **The audit chain.** `audit_log` is hash-chained and refuses UPDATE and DELETE at the database level. An
  alteration that verification does *not* detect is in scope.
- **Provenance.** Content credentials are read and preserved. A file that verifies as valid when it should
  read as tampered is in scope.

## What is not

- Findings that require database superuser access. The audit chain's own documentation is explicit that a
  superuser can drop the append-only rules; the chain detects that rather than preventing it. That is the
  honest limit, not a bug.
- Missing rate limits on a self-hosted deployment's own admin surface.
- Dependency advisories already carried with a written rationale in
  [`deny.toml`](deny.toml) — though an argument that one of those rationales is *wrong* is very much in scope.
- Anything found only in the `data/` fixtures or a dev-mode default. `.mise.toml` ships obvious local
  credentials on purpose; if you find a real one, that *is* a report.

## Supported versions

Pre-1.0 and in active development. Fixes land on `main`; there are no maintained release branches yet.
