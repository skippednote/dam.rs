# A grid that holds a hundred thousand assets and still answers the keyboard

The five posts before this one were about storage, rights, cost and correctness. This one is about the
part people actually touch, and about three decisions that turned out to constrain everything else: the
wire types are generated, the grid is a real ARIA grid, and accessibility was a release gate from the
first UI commit rather than a cleanup pass before launch.

The stack is Svelte 5 with runes, SvelteKit, Vitest for units, Playwright for browser tests, and axe for
accessibility assertions inside those browser tests.

## Generated types, or the API drifts

Every wire type in the frontend is generated from the repository's OpenAPI document. Nothing is
hand-written, and CI regenerates the file and fails if the result differs from what is committed.

The failure this prevents is mundane and constant: a Rust handler changes a field from required to
optional, the TypeScript still says required, and the mismatch surfaces as a runtime error weeks later
in a screen nobody thought was related. Generating the types moves that to a compile error in the same
change that caused it.

The check that the committed file is current matters as much as the generation. Without it, the
generated file drifts from the schema and becomes a second source of truth that happens to be stale —
which is worse than hand-written types, because everyone believes it.

This bit us in an instructive way. Our OpenAPI generator accepts duplicate schema names silently and the
last one wins. Two unrelated screens each defined a view type with the same name, and the symptom was
that one screen's generated type quietly changed shape. Not a build failure. A different type, with the
same name, that compiled.

## A virtualized ARIA grid with one roving tab stop

A media library grid has to hold a lot of tiles and stay navigable by keyboard. Those two requirements
fight.

Virtualization means only the visible rows exist in the DOM. Accessibility means the thing has to
announce itself as a grid with a known number of rows and columns, and a keyboard user has to be able to
move through it predictably. Do virtualization naively and a screen reader is told there are twelve
items when there are ninety thousand, and arrowing to row 400 moves focus to an element that does not
exist yet.

What that requires in practice:

**One roving tab stop.** The grid is a single tab stop. Tab moves into it and back out; arrow keys move
within it. The alternative — every tile being tabbable — means a keyboard user pressing Tab ninety
thousand times to reach the footer, which is technically navigable and practically not.

**The full set described, not the rendered subset.** ARIA row and column counts describe the whole
library. The rendered window is an implementation detail and must not leak into what assistive
technology is told.

**Focus that survives recycling.** When focus moves to a row outside the window, the window has to move
first and focus has to land on the element once it exists. Getting this wrong produces the worst failure
mode in the interface: focus falls back to `<body>`, and a keyboard user's position is silently lost.

None of this is novel. All of it is the kind of thing that is very hard to retrofit, because it
constrains the component's structure rather than its styling.

## Accessibility as a gate, from the first commit

Our design decisions document says WCAG 2.1 AA is a release gate "from the first UI commit." That
phrasing was deliberate and the reason is arithmetic rather than principle.

Add an accessibility gate to a mature UI and it fails immediately, with a backlog of violations across
every screen. The gate cannot go green until the backlog is cleared, the backlog is never the most
urgent thing, and so the gate is disabled "temporarily" and never re-enabled. This is the normal life
cycle of a retrofitted accessibility check and it ends with a badge in a README.

Added from the first commit, the gate is green because there is almost nothing to check, and every
subsequent change has to keep it green. The cost is paid per-change, when it is small and when the
person paying it is the person who introduced it.

So axe runs inside the Playwright suite, in both themes, and a violation fails the build.

Two things this caught that review would not have:

**Invented colour tokens.** A change used `text-on-accent`, `bg-canvas` and `text-danger` — plausible
names that do not exist in our token set, which meant they resolved to nothing and produced a contrast
failure. The real tokens are `text-accent-fg`, `bg-bg` and `text-state-rights-denied-fg`. The lesson is
that a design token system needs a check, because a wrong token name is invisible in a diff and produces
a subtly broken result rather than an error.

**Two panels with the same accessible name.** A sidebar ended up with two regions both labelled
"History", which is ambiguous to anyone navigating by landmark. Renaming one to "Previous holds" fixed
the violation and produced a better label for sighted readers too — which is the usual outcome, and the
best argument for the gate.

## Colour carries meaning exactly once

The interface has one rule about colour that shapes a surprising amount of it: **semantic colour belongs
to rights, and nothing else may use it.**

Every other state — processing, archived, superseded, held, draft — gets a label and a non-colour cue.
Rights alone get green and red.

Two reasons. The obvious one is that colour-blind users cannot be asked to distinguish six semantic
states by hue. The less obvious one is that if colour means six things, it means nothing, and the one
place a person genuinely needs an instant read is whether they may use this asset. Spending the strongest
signal in the interface on the one decision with legal consequences, and refusing to spend it anywhere
else, keeps that signal working.

Both palettes define the complete token set rather than one being a delta of the other, so a token that
exists in dark and not in light cannot silently fall back to something unstyled. Dark is the default,
because it is the right surface for reviewing images.

## Testing the boundary twice, from both sides

The browser tests mock the HTTP boundary. The Rust integration tests exercise the same routes against a
real PostgreSQL.

This is deliberate, and the alternative is worse in both directions. Browser tests against a live stack
are slow and flaky and fail for reasons that have nothing to do with the UI. Backend tests alone never
exercise what the UI actually sends. Testing each side against the shared contract — the same OpenAPI
document the types are generated from — keeps both honest without either waiting on the other.

The failure mode this design has is real and worth naming: a mock can drift from the thing it mocks.
Generated types close most of that gap, since a mock returning the wrong shape fails to typecheck. It
does not close all of it — a mock can return a well-typed response the server would never actually
produce.

## What we found by running it

Two things from this month, both about the tests rather than the interface.

The browser suite is genuinely flaky. Three full runs: an unchanged trunk lost 4 of 410, a branch
touching no frontend code lost 3, and the same branch run serially lost 1. **No test failed in more than
one of those runs.** All timeouts on visibility assertions in unrelated specs — timing, not broken
assertions. Running serially helped without curing it, which rules out worker contention as the whole
story.

We turned on retries in CI, and wrote down explicitly that this is instrumentation rather than a fix: a
test that passes on the second attempt is reported as flaky and the build goes green, and one that fails
three times is worth reading. Without retries, a random red is indistinguishable from a real regression,
and the reasonable response to a suite that cries wolf is to stop believing it. The debt is recorded
against the specific file that appears in all three lists.

And the unit tests were launching a browser that the *next* CI step installed. Three of the seven unit
files run in Vitest's browser mode. The step failed with "executable doesn't exist" after silently
running four of seven files. Invisible on a laptop, where the browser has been cached since the first
time anyone ran the suite.

That is the theme of the whole series, arriving one more time from a different direction: the tooling
was reporting a state that was not true, and it took running it somewhere clean to find out.

---

*Previous: [The library said the bytes were there and only the download
disagreed](05-the-library-said-the-bytes-were-there.md)*

*This is the last post in the series. The code is at
[github.com/skippednote/dam.rs](https://github.com/skippednote/dam.rs).*
