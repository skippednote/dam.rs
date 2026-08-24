**What this changes, and why.** The why is the part review needs; the what is in the diff.

**How it was verified.** Which gate you ran, and — if it is a behaviour change — the test that fails without
it. If you drove it against a running instance, say what you saw.

```
mise run check:all
```

**Anything you did not do.** Scope you left out, a case you know is not covered, a decision you were unsure
about. Saying so is faster than having it found in review, and it is not held against you.

<!--
  A note on comments: this codebase explains decisions, constraints, and the failures that motivated the
  code, rather than what the next line does. If you learned something the hard way while writing this,
  that sentence is worth more than the rest of the diff.
-->
