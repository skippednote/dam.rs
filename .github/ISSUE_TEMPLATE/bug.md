---
name: Bug report
about: Something behaves differently from how it is documented or intended
labels: bug
---

**What happened, and what you expected instead.**

**How to reproduce it.** The most useful version is the request, or the sequence of screens. If it involves
an asset, its media type and roughly its size matter more than the file itself.

**Which part.** `damd`, `dam-worker`, `damctl`, the Svelte app, or a migration.

**Version.** The commit you are on (`git rev-parse --short HEAD`), and whether you built it yourself or ran
the image.

**Anything in the logs.** Please redact keys, tokens and signed URLs — a signed URL is a credential.

<!--
  If it looks like a security problem — tenant isolation, delivery, credentials, the audit chain — please
  use private reporting instead of this template: SECURITY.md has the route.
-->
