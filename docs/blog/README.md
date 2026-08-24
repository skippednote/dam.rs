# dam.rs — a six-post series

Why we built a self-hosted, S3-backed digital asset manager, and what building it taught us.

The first four posts are about the problem: what the established platforms cost, why their archival
story tends to be unusable, where rights actually have to be enforced, and what changes when the storage
is yours. The last two are about the implementation.

No vendor is named. The problems described are structural — they follow from the pricing model and from
where the rights check sits — rather than being anybody's particular failing.

| # | Post | About |
|---|---|---|
| 1 | [Your DAM bill grows with your library. Your library only grows.](01-your-bill-grows-with-your-library.md) | The four meters every platform bills on, why deletion isn't an escape hatch, and what leaving costs |
| 2 | [Cold storage you can't search is a filing cabinet in a warehouse](02-cold-storage-you-cant-search.md) | Why archival tiering usually breaks the library, and the minimum-duration arithmetic that isn't in the proposal |
| 3 | [The green badge is not permission](03-the-green-badge-is-not-permission.md) | Rights displayed versus rights enforced, and everything a signed URL has to carry |
| 4 | [Your bucket, your keys, your bill](04-your-bucket-your-keys-your-bill.md) | What S3-compatible actually buys, BYOK, content addressing, and where self-hosting is the wrong call |
| 5 | [The library said the bytes were there and only the download disagreed](05-the-library-said-the-bytes-were-there.md) | Three defects a week of load testing found, and the belief they had in common |
| 6 | [A grid that holds a hundred thousand assets and still answers the keyboard](06-a-grid-that-holds-a-hundred-thousand-assets.md) | The Svelte implementation, generated types, and accessibility as a gate rather than a cleanup |

## Numbers used in the series

Every figure comes from a run against this repository rather than from a vendor's documentation:

- **20 cases passed, 0 skipped** and a **76.7-second** Glacier restore, against real AWS S3 in
  `ap-south-1`.
- **608 objects lost and ~80 truncated** when a disk filled during a load run, with the database
  reporting every one of them `active` at its recorded size.
- **1,280 indexing jobs** never claimed once in thirty-five minutes, leaving roughly half the library
  unsearchable while sitting in the database.
- **2,558 object keys** across five tenants, each verified to sit under its own tenant's prefix.
- A delivery token naming another tenant returning **302 — the asset served** — once the tenant check is
  removed.
- **4, 3 and 1 of 410** browser tests failing across three full runs, with no test failing in more than
  one of them.

## A note on what these posts admit

Several of them describe things we got wrong, and one describes a check that still does not catch the
case it was written for. That is deliberate. A series about a storage system that only described its
successes would be making exactly the claim the fifth post argues against — that a system's silence
about its own faults is evidence there are none.
