# A grid that holds a hundred thousand assets and still answers the keyboard

The first 100,000-asset grid test passed while proving less than its name suggested. The component rendered a bounded set of cells, exposed `aria-rowcount="25000"`, and moved one roving tab stop with the arrow keys. The real `/assets` route, however, fetched only 60 records. A scrollbar sized for the full result set could therefore lead into blank virtual rows. The accessibility metadata was more complete than the data path behind it.

> [!TLDR]
> An accessible virtualized asset grid needs three contracts to agree: the API must expose stable windowed data and a total, the component must virtualize DOM rows while reporting absolute ARIA positions, and keyboard focus must trigger loading before it moves outside the current window. dam.rs has strong component semantics, generated OpenAPI types, roving focus, and automated WCAG gates, but its current browse route still needs window fetching before the full 100,000-asset claim is true end to end.

That gap is more useful than a polished success story. Accessibility and scale are both consistency problems. The visible tile, the loaded page, the virtual row, the announced row count, the focused cell, and the URL all describe one collection. When any two disagree, the interface can look fine while becoming unusable to someone navigating differently.

The frontend stack is Svelte 5 with runes, SvelteKit, TanStack Virtual, Vitest browser mode, Playwright, and axe. The interesting engineering is not the list of tools. It is where each tool's proof stops.

## One contract from Rust to TypeScript

The backend generates `openapi.json` from Rust annotations. The frontend generates `schema.d.ts` from that document and imports every wire type from the generated module. No component re-declares `AssetSummary`, `AssetPage`, or the rights and storage unions by hand.

```press-diagram
{"type":"flow","title":"Wire contract","stages":[{"label":"Rust DTOs"},{"label":"OpenAPI"},{"label":"schema.d.ts"},{"label":"typed client"},{"label":"Svelte UI"}],"footer":"CI regenerates both artifacts and fails on drift."}
```

The checked-in client makes generated code reviewable, but checked-in generation creates a stale-artifact risk. CI closes that loop by regenerating and failing on a diff:

```yaml
- run: cargo run -q -p damctl -- openapi --write
- run: >-
    pnpm exec openapi-typescript ../openapi.json
    -o src/lib/api/schema.d.ts
- name: fail if the generated API client is stale
  run: git diff --exit-code -- src/lib/api/schema.d.ts
```

This catches a common class of drift. If Rust changes a required field to optional, the generated TypeScript forces callers to handle `undefined` in the same change. If a status union gains a variant, exhaustive UI branches stop compiling instead of silently rendering nothing.

Generation does not guarantee semantic uniqueness. An earlier generator accepted duplicate schema names and allowed the later definition to win. Both definitions were individually valid and the resulting TypeScript compiled. The correction is to treat component names in the OpenAPI document as a global namespace and add a schema-level uniqueness assertion, not to trust generation as a magic word.

Nor does a type prove a response can occur. A mock can return a well-typed state the real server never produces, or omit the sequence in which states arrive. Shared types eliminate shape drift; they do not eliminate behavioural drift.

## Virtualization removes the DOM that accessibility reads

A media grid cannot mount 100,000 image cells and remain responsive. It also cannot tell assistive technology that the collection contains only the cells currently mounted.

The reconciliation uses two levels of truth:

- the grid exposes the total row and column counts for the complete collection;
- each rendered row exposes its absolute one-based `aria-rowindex`;
- only the visible rows plus overscan exist in the DOM;
- one grid cell has `tabindex="0"`; every other rendered cell has `-1`.

The component markup expresses that directly:

```svelte
<div
  bind:this={viewport}
  role="grid"
  aria-label="Assets"
  aria-rowcount={Math.ceil(total / columns)}
  aria-colcount={columns}
  aria-multiselectable="true"
  onkeydown={onkeydown}
>
  {#each virtualRows as virtualRow (virtualRow.index)}
    <div role="row" aria-rowindex={virtualRow.index + 1}>
      <!-- rendered cells for this absolute row -->
    </div>
  {/each}
</div>
```

If `aria-rowcount` used the rendered window, a screen reader could announce a 100,000-asset library as twenty items with no visual symptom. If `aria-rowindex` restarted at one after every window shift, each scroll position would claim to be the beginning of the library.

The sizer element uses the total row count so the scrollbar represents the complete collection. A browser-mode test measures the resulting layout rather than comparing the serialized style string, because Chrome can normalize a large pixel value into exponential notation while laying it out correctly.

That test pins a platform behaviour the implementation depends on: a 3,000,000-pixel height must survive the CSSOM round trip. It is not glamorous, but neither is discovering that the final third of a library cannot be reached because a browser clamped the scroll range.

## A roving tab stop makes a grid navigable

Making every tile tabbable is technically keyboard-accessible and practically hostile. A user would have to press Tab through the collection to reach the next control on the page.

The WAI-ARIA grid pattern uses one tab stop. Tab enters and leaves the composite widget. Arrow keys move within it. Home and End move within the current row; Control or Command plus Home and End move to the collection boundaries. Space changes selection and Enter activates the focused asset.

dam.rs stores the focused item as an index inside the loaded window. Moving focus scrolls the virtual row into view, then waits until Svelte has mounted it before calling `focus()`:

```ts
function move(next: number) {
  focused = Math.min(Math.max(next, 0), items.length - 1);
  const row = Math.floor((focused + offset) / columns);
  $virtualizer?.scrollToIndex(row);
  queueMicrotask(() => {
    const target = viewport?.querySelector<HTMLElement>(
      `[data-cell-index="${focused}"]`
    );
    target?.focus();
  });
}
```

The order matters. Focusing before the window moves targets an element that does not exist. If the browser then falls back to `<body>`, a keyboard user loses position without an error message.

Arrow navigation holds at row edges rather than wrapping. Moving right from the last column to the first column of the next row creates a large visual jump. Holding position matches the spatial model and prevents the browser from scrolling the page underneath the grid.

Selection is separate from focus. `aria-selected` lives on each grid cell, while a polite live region announces counts such as "4 of 120 assets selected." A sighted user can read the bulk bar. A screen-reader user needs an equivalent signal that the Space key changed state.

The favourite star is not another tab stop. Pressing `f` on the focused cell toggles it, except when Control, Command, or Alt is held so the component does not steal the browser's Find command. That is a small example of why keyboard parity is more than adding `onkeydown` to whatever is clickable.

## The 60-item boundary the component cannot solve

The grid API already carries the right window vocabulary:

- `items` contains the records currently loaded;
- `total` contains the complete matching count;
- `offset` identifies the absolute index of `items[0]`.

The current `/assets` route calls `listAssets({ limit: 60 })` or `searchAssets({ limit: 60 })` once and passes the server's full `total` into the grid. There is no range-change callback that asks the page to load a new offset as virtual rows approach the edge.

That means the component can truthfully announce the full count and render a bounded window, but the page cannot populate rows beyond the first request. The scrollbar's geometry describes data the route has not loaded.

This is the missing end-to-end mechanism:

```press-diagram
{"type":"sequence","title":"Windowed grid loading","actors":["grid","page","API","cache"],"messages":[{"from":0,"to":1,"label":"range changed"},{"from":1,"to":3,"label":"check window"},{"from":3,"to":1,"label":"cache miss","reply":true},{"from":1,"to":2,"label":"limit offset"},{"from":2,"to":1,"label":"items total","reply":true},{"from":1,"to":3,"label":"store window"},{"from":1,"to":0,"label":"items offset","reply":true}]}
```

A robust implementation needs more than calling the API on every scroll event.

### Fetch ranges, not pixels

The virtualizer should report the absolute item range needed, plus overscan. The page converts that range into stable API windows such as 60-item pages and coalesces duplicate requests.

### Keep result identity with the cache key

Windows for `query=A, order=newest` cannot be reused for `query=B` or another sort. Query, filters, order, limit, and offset all belong to the cache identity. Changing the result set must also clear selection because hidden selected IDs could otherwise remain armed for a bulk action.

### Preserve focus through an unloaded row

When an arrow targets a cell outside the loaded window, the grid needs a pending absolute focus index. It scrolls, requests the containing page, renders it, and then focuses the cell. Moving focus only within `items.length` avoids a crash but traps keyboard users in the first page.

### Handle failures without losing position

If the page request fails, the current cell should retain focus and the failure should be announced. Falling back to the first cell turns a transient network error into lost navigation context.

Until those pieces exist, "100,000" is a component stress fixture and an ARIA contract, not proof that the shipped browser can traverse 100,000 records. That is the correct proof boundary to publish.

## Accessibility as a release gate, not a finishing pass

The design decision in dam.rs is WCAG 2.1 AA from the first UI commit. The timing matters because retrofitting a gate to a mature interface creates a backlog large enough that the gate is usually disabled while people promise to return later.

Starting green makes accessibility a per-change cost. The person adding a dialog also adds its name, focus behaviour, escape path, contrast, and browser test while the context is fresh.

Playwright runs axe against the application and against a `/style` reference page that renders every semantic badge in light and dark themes. The suite forces both colour schemes. Testing only Chromium's default theme would leave half the token set unmeasured.

Automated scanning is explicitly treated as incomplete. Axe cannot tell whether the skip link is the first keyboard stop, whether activating it moves focus to `main`, whether the page has a useful heading hierarchy, or whether a live result update is announced at the right moment. Those properties have direct browser assertions.

The gate checks, among other things:

- exactly one main landmark and one `h1`;
- a visible-on-focus skip link that moves actual focus;
- document language;
- a viewport that permits zoom;
- semantic states in both themes;
- roving focus through real keyboard input;
- selection and error announcements;
- zero-result content rendered as a status sentence rather than an empty ARIA grid.

That last case came from an empty fixture. A `role="grid"` containing prose but no rows fails required-child semantics and announces the wrong object. The correct empty state is not a grid with zero rows. It is a sentence answering the search.

## Colour gets one legal meaning

The interface reserves strong semantic green and red for rights. Processing, archive, draft, superseded, and hold states use labels, icons, borders, or neutral treatments.

This is partly a colour-vision decision and partly an information hierarchy decision. If six unrelated dimensions use green and red, the fastest signal no longer answers the most consequential question: may this asset be distributed under the stated terms?

Every state still has text. Colour is redundant, not the only carrier. Both themes define complete token sets so a foreground cannot silently fall back to a value designed for the other surface.

The gate once caught plausible token names that did not exist. CSS accepted them as absent styles rather than syntax errors, leaving contrast to the cascade. A design-token vocabulary needs static or rendered verification because a misspelled variable often produces a subtly wrong page instead of a failed build.

## Test both sides of the HTTP boundary

Frontend browser tests mock HTTP so they can exercise UI state deterministically. Rust integration tests call real routes against Postgres. Both sides are typed from the OpenAPI contract.

This split keeps browser tests fast enough to run often and backend tests capable of proving SQL, policy, and status behaviour. It still leaves a seam: mocks can return a valid shape with impossible semantics. A small real-stack smoke suite is the appropriate third layer for critical journeys such as connect, browse, select, download, and restore.

Historical CI work exposed two problems in the web gate itself. In three full runs of the then 410-test browser suite, unrelated visibility assertions failed 4, 3, and 1 times, with no individual test failing in more than one run. Those numbers describe that diagnostic run, not the current suite count. Retries were enabled in CI as instrumentation so a passed retry is reported as flaky. They reduce noise but do not repair timing.

The unit step also launched Chromium before the workflow installed it. Four non-browser files ran, then the browser-mode files failed because the executable did not exist. A developer machine already had the browser cached, so local runs concealed the ordering mistake. Installation now precedes Vitest in both CI and the documented local gate.

## What remains expensive

Windowed virtualization complicates data fetching, cache invalidation, selection, and focus. A normal paginated list is easier to test and may be better when users browse sequential pages rather than spatially scan media. The grid earns its complexity only because image review benefits from a stable visual field.

Automated accessibility checks also impose maintenance. Browser and axe upgrades can change findings. Semantic assertions can become coupled to implementation. The answer is not to weaken them, but to keep each test tied to a user-observable property and delete tests that merely mirror component internals.

The current page-level data gap is the largest open item. Component tests with `total=100_000` must not be used as evidence that the product can fetch and navigate that collection. The next useful test starts near an unloaded boundary, presses ArrowDown, observes one range request, and verifies that focus lands on the newly mounted absolute cell without resetting selection or scroll.

## FAQ

### How do you make a virtualized grid accessible?

Expose total row and column counts on the grid, absolute row and column indices on rendered items, one roving tab stop, and focus logic that scrolls and loads a target before focusing it. The rendered DOM window must not be presented as the full collection.

### Why not make every asset tile tabbable?

Tab is for entering and leaving the composite grid. Arrow keys are for moving inside it. Making every tile tabbable forces a keyboard user to traverse every asset before reaching the next page control.

### Do generated OpenAPI types prevent frontend and backend drift?

They prevent many shape mismatches when CI regenerates and checks the artifact. They do not prove that schema names are unique, that mocks represent possible server behaviour, or that state transitions happen in the same order.

### Can the current dam.rs page browse 100,000 assets?

Not yet end to end. The grid component keeps DOM size bounded and describes a 100,000-item total correctly, but the route currently loads one 60-item page. The claim becomes real only when absolute range loading, cache identity, pending focus, and failure recovery agree with the ARIA model.

An accessible large-data component is not finished when its attributes look right. It is finished when the data source, virtual window, announced position, and keyboard focus all tell the same story.
