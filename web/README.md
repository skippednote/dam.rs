# dam.rs frontend

The operator interface for dam.rs: a Svelte 5 and SvelteKit application generated against the repository's
OpenAPI document.

## Development

From the repository root, start the local dependencies, provision the development tenant, and run the API
and worker as described in the [root README](../README.md). Then:

```sh
pnpm install --frozen-lockfile
pnpm run dev
```

Visit Settings and paste the API key printed by `mise run dev:seed`. The browser stores this development
key locally; this is not the intended production authentication model.

## Checks

```sh
pnpm run check
pnpm run lint
pnpm exec vitest run
pnpm exec playwright test
pnpm run build
```

The browser tests mock the HTTP boundary while Rust integration tests exercise the same routes against
PostgreSQL. Wire types are generated from `../openapi.json`:

```sh
pnpm run gen:api
```

## Interface conventions

- Svelte runes are used for component state.
- The asset browser is a virtualized WAI-ARIA grid with one roving tab stop.
- Every state uses a label and a non-colour cue; rights alone own semantic colour.
- Both dark and light palettes define the complete token set in `src/routes/layout.css`.
- Dark is the default image-review surface. Light remains supported for embedded contexts.
- Internal links use SvelteKit's resolved paths so configured base paths remain valid.
- The API key is never rendered in full; only the audit-safe prefix appears in the shell.

The approved identity and UI direction is documented in [the brand guide](../docs/brand/README.md).
