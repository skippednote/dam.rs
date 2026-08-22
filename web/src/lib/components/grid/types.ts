/**
 * The grid's data shape, taken from the generated OpenAPI client.
 *
 * Re-exported rather than redeclared so the chain F.3 established stays unbroken: database CHECK →
 * `dam-core` → `openapi.json` → `schema.d.ts` → here. Adding a field to `AssetSummary` in Rust makes
 * it available here with no edit; removing one the grid reads breaks the build.
 */
import type { components } from '$lib/api/schema';

export type AssetSummary = components['schemas']['AssetSummary'];
export type AssetPage = components['schemas']['AssetPage'];
