/**
 * The four-dimension state vocabulary (F.2).
 *
 * An asset carries four independent states at once — where its bytes live, whether it may be used,
 * whether its provenance survived, and how much a model trusts its own tags — and a grid cell has to
 * show all four without becoming a colour puzzle. The vocabulary assigns each dimension a different
 * *channel*:
 *
 * | Dimension | Channel | Why that channel |
 * |---|---|---|
 * | Tier | form — icon and border | Must not compete with rights for the colour channel |
 * | Rights | semantic colour | The only dimension with legal consequence, so it gets the loudest one |
 * | Provenance | neutral | A missing credential is a fact, not an alarm |
 * | Confidence | magnitude bars | It is a quantity, and quantities read as length |
 *
 * These tests pin the *channel assignment*, not the pixels. A change that gives tier its own colour
 * is not a restyle — it takes the channel rights depends on, and the failure is that a cleared asset
 * and an archived asset start to look alike.
 */
import { describe, expect, it } from 'vitest';
import {
	RIGHTS_STATES,
	PROVENANCE_STATES,
	TIERS,
	confidenceLabel,
	provenanceMeta,
	rightsMeta,
	tierMeta,
	clampConfidence
} from './vocabulary';

describe('the vocabularies match what the database can say', () => {
	it('covers every rights_state in the schema', () => {
		// `assets.rights_state` CHECKs exactly these. A UI that cannot render one of them shows an
		// asset with no rights indicator at all, which reads as "no restriction".
		expect([...RIGHTS_STATES]).toEqual(['allowed', 'expiring', 'denied', 'unknown']);
	});

	it('covers every provenance_state in the schema', () => {
		expect([...PROVENANCE_STATES]).toEqual(['none', 'valid', 'invalid', 'untrusted']);
	});

	it('covers the tiers a placement can be in', () => {
		expect([...TIERS]).toEqual(['hot', 'cool', 'archive', 'restoring', 'restored']);
	});
});

describe('colour is never the only signifier (WCAG 1.4.1)', () => {
	it('gives every state in every dimension a text label', () => {
		for (const state of RIGHTS_STATES) {
			expect(rightsMeta(state).label.length, `rights:${state}`).toBeGreaterThan(0);
		}
		for (const state of PROVENANCE_STATES) {
			expect(provenanceMeta(state).label.length, `provenance:${state}`).toBeGreaterThan(0);
		}
		for (const tier of TIERS) {
			expect(tierMeta(tier).label.length, `tier:${tier}`).toBeGreaterThan(0);
		}
	});

	it('gives every state a distinct label, so two states never read identically', () => {
		const labels = RIGHTS_STATES.map((s) => rightsMeta(s).label);
		expect(new Set(labels).size).toBe(labels.length);
	});
});

describe('tier is encoded in form, not colour', () => {
	it('uses one neutral colour token across every tier', () => {
		// The channel assignment, asserted directly. If tier ever takes a colour of its own it is
		// competing with rights, and a grid where "archived" is amber and "expiring" is amber is a
		// grid where nobody trusts either.
		const tokens = new Set(TIERS.map((t) => tierMeta(t).colorToken));
		expect(tokens.size, `tier colour tokens: ${[...tokens].join(', ')}`).toBe(1);
	});

	it('distinguishes the tiers by icon and border instead', () => {
		const shapes = TIERS.map((t) => `${tierMeta(t).icon}/${tierMeta(t).border}`);
		expect(new Set(shapes).size).toBe(TIERS.length);
	});

	it('marks the two tiers that cannot serve bytes immediately', () => {
		// The distinction a user acts on: everything else is a download, archive is a request and a
		// wait. It has to be derivable rather than inferred from the label text.
		expect(tierMeta('archive').needsRestore).toBe(true);
		expect(tierMeta('restoring').needsRestore).toBe(true);
		for (const tier of ['hot', 'cool', 'restored'] as const) {
			expect(tierMeta(tier).needsRestore, tier).toBe(false);
		}
	});
});

describe('rights own the semantic colour channel', () => {
	it('gives each rights state its own colour token', () => {
		const tokens = RIGHTS_STATES.map((s) => rightsMeta(s).colorToken);
		expect(new Set(tokens).size).toBe(tokens.length);
	});

	it('never styles unknown as though it were allowed', () => {
		// The dangerous default. `rights_state` starts at 'unknown', and the schema comment on the AI
		// gate is explicit that unknown is not permission — "never dispatch one whose rights are
		// simply unknown". A UI that renders unknown like allowed converts an unevaluated asset into
		// an apparently cleared one.
		expect(rightsMeta('unknown').colorToken).not.toBe(rightsMeta('allowed').colorToken);
		expect(rightsMeta('unknown').label).not.toBe(rightsMeta('allowed').label);
		expect(rightsMeta('unknown').blocksDistribution).toBe(true);
		expect(rightsMeta('allowed').blocksDistribution).toBe(false);
	});

	it('treats denied and unknown as blocking, and expiring as not yet blocking', () => {
		expect(rightsMeta('denied').blocksDistribution).toBe(true);
		expect(rightsMeta('expiring').blocksDistribution).toBe(false);
	});
});

describe('provenance is neutral', () => {
	it('uses one neutral token for every provenance state', () => {
		const tokens = new Set(PROVENANCE_STATES.map((s) => provenanceMeta(s).colorToken));
		expect(tokens.size).toBe(1);
	});

	it('never borrows a rights colour', () => {
		// A broken credential is a fact about the file's history. Painting it in the rights palette
		// makes it look like a licensing problem, which sends the wrong person to investigate.
		const rightsTokens = new Set(RIGHTS_STATES.map((s) => rightsMeta(s).colorToken));
		for (const state of PROVENANCE_STATES) {
			expect(rightsTokens.has(provenanceMeta(state).colorToken), `provenance:${state}`).toBe(false);
		}
	});

	it('still distinguishes its states by icon and label', () => {
		const icons = PROVENANCE_STATES.map((s) => provenanceMeta(s).icon);
		expect(new Set(icons).size).toBe(PROVENANCE_STATES.length);
	});
});

describe('confidence is a magnitude', () => {
	it('clamps anything outside 0..1 rather than overflowing a bar', () => {
		expect(clampConfidence(-0.5)).toBe(0);
		expect(clampConfidence(1.7)).toBe(1);
		expect(clampConfidence(0.42)).toBeCloseTo(0.42);
	});

	it('treats a missing or non-numeric confidence as unknown rather than zero', () => {
		// A null confidence is a tag nobody scored — often a human's. Rendering it as 0% would say the
		// model was certain it was wrong, which is the opposite of what the data means.
		expect(clampConfidence(null)).toBeNull();
		expect(clampConfidence(undefined)).toBeNull();
		expect(clampConfidence(Number.NaN)).toBeNull();
	});

	it('labels the value in words as well as in length', () => {
		// The bar carries the magnitude; the label is what a screen-reader user and a monochrome
		// printout get.
		expect(confidenceLabel(0.91)).toContain('91');
		expect(confidenceLabel(null)).toMatch(/not scored/i);
	});
});
