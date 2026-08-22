/**
 * The runtime half of the drift gate (F.3).
 *
 * TypeScript catches drift at compile time — an added variant breaks the exhaustive `Record`, a
 * removed one breaks the `satisfies` on the array. Neither catches a *generated file that is stale*:
 * if `schema.d.ts` was not regenerated after the Rust enum changed, both checks pass against an
 * out-of-date union and the UI is confidently wrong.
 *
 * So this reads `openapi.json` itself — the artifact the backend test suite asserts is current — and
 * compares it with what the UI actually renders. The chain is then complete: database CHECK →
 * `dam-core` enum → `openapi.json` → `schema.d.ts` → these constants, with a test at each hop.
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { PROVENANCE_STATES, RIGHTS_STATES } from '$lib/components/state/vocabulary';

interface OpenApiDocument {
	components: { schemas: Record<string, { enum?: string[] }> };
}

const document = JSON.parse(
	readFileSync(new URL('../../../../openapi.json', import.meta.url), 'utf8')
) as OpenApiDocument;

function schemaEnum(name: string): string[] {
	const schema = document.components.schemas[name];
	if (!schema?.enum) {
		throw new Error(
			`openapi.json has no enum schema named ${name}; found ${Object.keys(document.components.schemas).join(', ')}`
		);
	}
	return schema.enum;
}

describe('the UI renders exactly the states the API can produce', () => {
	it('covers every RightsState in the document', () => {
		// If this fails, either the generated client is stale or a variant has no badge — and a state
		// with no badge renders as nothing, which reads as "no restriction".
		expect([...RIGHTS_STATES].sort()).toEqual([...schemaEnum('RightsState')].sort());
	});

	it('covers every ProvenanceState in the document', () => {
		expect([...PROVENANCE_STATES].sort()).toEqual([...schemaEnum('ProvenanceState')].sort());
	});

	it('reads a document that actually has schemas, so the comparison is not vacuous', () => {
		// Without this, a malformed or empty document would make both assertions above compare two
		// empty lists and pass.
		expect(Object.keys(document.components.schemas).length).toBeGreaterThan(4);
	});
});
