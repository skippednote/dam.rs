# Contributing

Thanks for looking. This is a young project and the most useful contributions right now are bug reports with
a reproduction, and small changes that come with the test that would have caught the bug.

## Getting it running

Everything is pinned through [mise](https://mise.jdx.dev), including libvips and ffmpeg — the media suites
assert which loaders their build provides rather than guessing, so an `apt` libvips is a different build and
will fail tests that pass here.

```sh
mise install
mise run up          # Postgres + SeaweedFS in Docker
mise run dev:seed    # prints a development API key, once
mise run dev:api     # each of these in its own terminal
mise run dev:worker
mise run dev:web
```

The worker is not optional. Uploads sit in staging until it verifies, promotes, derives and indexes them, so
without it the application looks broken in a way that is easy to mistake for a bug.

## The gates

```sh
mise run check       # fmt, clippy, tests
mise run check:deny  # advisories, licences, bans, sources
mise run check:web   # typecheck, lint, unit, browser and accessibility suites
mise run check:all   # all three, which is what CI runs
```

Clippy runs with `-D warnings`, and `unwrap`/`expect`/`dbg!` are denied in library code. Tests may use them;
that is what the `#![allow(...)]` at the top of each test file is for.

Database and object-store tests each start their own container through testcontainers, so there is no shared
fixture to reset and the suites run in parallel. They need Docker running and nothing else.

## What a good change looks like

**A test that fails before and passes after.** Not for its own sake — the useful ones assert a *property*
rather than an implementation. `crates/dam-db/tests/audit.rs` is a fair example: it does not check that a
function returns a value, it checks that altering a row is detected and that removing one is reported
differently, because those are the two things somebody needs from a hash chain.

**A comment that says why, where the why is not obvious.** This codebase is unusually heavily commented and
that is deliberate. The convention is that a comment explains a decision, a constraint, or a failure that
motivated the code — not what the next line does. If you found something out the hard way, write that down;
it is the most valuable part of the change.

**One logical change.** Formatting churn mixed into a behaviour change makes the behaviour change unreviewable.

**Honest reporting.** If part of it does not work, say so in the pull request. A description that overstates
what was verified costs more to unwind than the bug did.

## What to expect

Review is by one maintainer, so it may not be immediate. A change that arrives with its test and a clear
description of the property it defends is much faster to merge than one that does not.

If you are about to build something large, open an issue first. [TASKS.md](TASKS.md) is the actual
implementation queue with the reasoning attached, and it will usually tell you whether something is unbuilt
because nobody has got to it or because it is waiting on a decision.

## Conduct

Be decent to people. Disagree about the work, not about each other; assume the other person is trying to get
it right. If somebody's behaviour is making this project worse to be part of, tell the maintainer privately
through a [GitHub security advisory](https://github.com/skippednote/dam.rs/security/advisories/new) — it is
the only private channel this repository has, and it works for this too.

That is deliberately short. A solo-maintained project publishing a formal enforcement ladder would be
describing a committee that does not exist.
