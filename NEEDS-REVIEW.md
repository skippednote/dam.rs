# Nothing is open

Every question this file carried was answered on 2026-08-21: the five access-control semantics, whether an
administrator may read a private comment, what a portal backed by a live query publishes, and C2PA's signing
identity and inbound-failure policy. What was chosen — and what it cost to implement — is in `DECISIONS.md`
under **The parked decisions, answered 2026-08-21**.

Two sections that used to live here are gone for other reasons. Task 1.6's TUS surface was blocked on there
being no authentication layer; there is one, and the endpoints are built and tested
(`crates/dam-api/tests/tus.rs`). The `admin` role's decorative wildcards were a bug rather than a decision, and
`dam_core::policy::grants_permission` now expands them.

The file stays because the shape is worth keeping: when something is genuinely not mine to decide — an
irreversible disclosure, a compliance posture, a promise to a user that code cannot walk back — it goes here
with the options, the recommendation, and what proceeding without an answer would cost. An empty file is the
state to aim for, not a sign the practice stopped.

## What is still deliberately not built, and why

These are not open questions; they are recorded scope, so nobody has to rediscover the reason.

- **Rule-based asset groups** are evaluated live by decision, and the renderer refuses one by name rather than
  ignoring it. Nothing can create one yet — there is no group-administration surface — so live evaluation is
  built alongside that surface rather than now, for a path nothing can reach. Ignoring a rule would silently
  grant *less* access than an administrator configured, which is why the refusal is loud.
- **C2PA (task 1.9)** is unblocked and unbuilt: one signing identity per deployment, test certificates refused
  outside development, and a failed inbound manifest accepted, recorded and not re-signed. The mechanism, the
  crate and the schema were already settled.
- **An administrator's disclosure path for private comments** does not exist. The strict rule holds until a
  deliberate, audited path is built — not by widening a role that already exists.
