/**
 * Composing the shorthand query.
 *
 * The rail writes the same language the server parses, so the cases here are the ones where a naive string
 * concatenation changes the meaning of a query: a value with a space, a value with a quote, and a
 * hand-typed clause this module does not own and must not rewrite.
 */
import { describe, expect, it } from 'vitest';
import {
	hasTerm,
	parse,
	renderTerm,
	selectedValue,
	toggleTerm,
	withOnlyTerm,
	withTerm,
	withoutTerm
} from './query';

describe('rendering a term', () => {
	it('leaves a simple value alone', () => {
		expect(renderTerm({ key: 'brand', value: 'acme' })).toBe('brand:acme');
	});

	it('quotes a value with a space, because otherwise it becomes two terms', () => {
		// `brand:Acme Corp` parses as a brand filter plus the free text "Corp", which returns the wrong
		// assets and looks like a search bug rather than a quoting one.
		expect(renderTerm({ key: 'brand', value: 'Acme Corp' })).toBe('brand:"Acme Corp"');
	});

	it('escapes a quote inside a value', () => {
		expect(renderTerm({ key: 'caption', value: 'the "good" one' })).toBe(
			'caption:"the \\"good\\" one"'
		);
	});

	it('quotes an empty value rather than emitting a bare colon', () => {
		expect(renderTerm({ key: 'brand', value: '' })).toBe('brand:""');
	});
});

describe('parsing', () => {
	it('separates terms from free text', () => {
		const { terms, text } = parse('brand:acme harbour photos colours:blue');
		expect(terms).toEqual([
			{ key: 'brand', value: 'acme' },
			{ key: 'colours', value: 'blue' }
		]);
		expect(text).toBe('harbour photos');
	});

	it('reads a quoted value back as one value', () => {
		expect(parse('brand:"Acme Corp"').terms).toEqual([{ key: 'brand', value: 'Acme Corp' }]);
	});

	it('leaves a range clause as text rather than claiming it', () => {
		// This module owns equality terms only. Treating `year:>2020` as a term would let a facet click
		// rewrite it — and a user's hand-typed range disappearing when they tick a checkbox is worse than
		// the rail simply not managing it.
		const { terms, text } = parse('year:>2020 brand:acme');
		expect(terms).toEqual([{ key: 'brand', value: 'acme' }]);
		expect(text).toBe('year:>2020');
	});

	it('preserves anything it does not recognise', () => {
		const query = '(brand:acme OR brand:globex) -colours:red';
		expect(parse(query).text).toContain('-colours:red');
	});
});

describe('toggling', () => {
	it('adds a term that is absent and removes one that is present', () => {
		const term = { key: 'brand', value: 'acme' };
		const added = toggleTerm('harbour', term);
		expect(added).toBe('brand:acme harbour');
		expect(hasTerm(added, term)).toBe(true);

		const removed = toggleTerm(added, term);
		expect(hasTerm(removed, term)).toBe(false);
		expect(removed).toBe('harbour');
	});

	it('does not add the same term twice', () => {
		// A double click on a facet, or a rail rendered from a stale query. Two identical terms are
		// harmless to the parser and make the box look broken.
		const term = { key: 'brand', value: 'acme' };
		const once = withTerm('', term);
		expect(withTerm(once, term)).toBe(once);
	});

	it('keeps the free text when a term is removed', () => {
		expect(withoutTerm('brand:acme harbour at dawn', { key: 'brand', value: 'acme' })).toBe(
			'harbour at dawn'
		);
	});

	it('matches a quoted term by its value rather than its rendering', () => {
		// The rail holds `{key, value}`; the box holds a string. If `hasTerm` compared renderings, a
		// checkbox for "Acme Corp" would read as unticked on a query that already filtered by it.
		const term = { key: 'brand', value: 'Acme Corp' };
		const query = withTerm('', term);
		expect(query).toBe('brand:"Acme Corp"');
		expect(hasTerm(query, term)).toBe(true);
	});

	it('puts terms before free text so the box reads as filters plus a search', () => {
		expect(withTerm('harbour', { key: 'colours', value: 'blue' })).toBe('colours:blue harbour');
	});
});

describe('browsing a hierarchy, as against ticking facets', () => {
	it('replaces the previous category rather than adding a second one', () => {
		// `in:a in:b` is legal and means "filed in both", which is almost never what somebody clicking through
		// a tree wants — it usually returns nothing while looking like the tree is broken. Navigating replaces.
		const first = withOnlyTerm('', 'in', 'exterior');
		expect(first).toBe('in:exterior');

		const second = withOnlyTerm(first, 'in', 'interior');
		expect(second).toBe('in:interior');
		expect(parse(second).terms).toHaveLength(1);
	});

	it('leaves other terms and hand-typed text alone', () => {
		const query = withOnlyTerm('brand:acme year:>2020 harbour', 'in', 'exterior');
		// The range clause this module does not own survives verbatim, and so does the free text.
		expect(query).toContain('year:>2020');
		expect(query).toContain('harbour');
		expect(query).toContain('brand:acme');
		expect(query).toContain('in:exterior');
	});

	it('clears the selection with null', () => {
		expect(withOnlyTerm('in:exterior brand:acme', 'in', null)).toBe('brand:acme');
	});

	it('reports which branch is selected, so the tree can highlight it', () => {
		expect(selectedValue('brand:acme in:exterior.yellow', 'in')).toBe('exterior.yellow');
		expect(selectedValue('brand:acme', 'in')).toBeNull();
	});
});
