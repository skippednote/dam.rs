/**
 * Composing the shorthand query.
 *
 * The rail writes the same language the server parses, so the cases here are the ones where a naive string
 * concatenation changes the meaning of a query: a value with a space, a value with a quote, and a
 * hand-typed clause this module does not own and must not rewrite.
 */
import { describe, expect, it } from 'vitest';
import {
	composeConditions,
	composeFilenames,
	hasTerm,
	narrow,
	operatorsFor,
	parse,
	renderCondition,
	renderTerm,
	selectedValue,
	toggleTerm,
	withOnlyTerm,
	withTerm,
	withoutTerm,
	type Condition
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

	it('leaves an interior hyphen unquoted, because it is an ordinary character', () => {
		// Found live: every pasted filename came back as `filename:"sample-003.jpg"`, which reads like an escape
		// somebody should worry about in a box whose whole job is to be readable.
		expect(renderTerm({ key: 'filename', value: 'sample-003.jpg' })).toBe(
			'filename:sample-003.jpg'
		);
		// A leading one is the "is empty" operator, so it still gets quoted.
		expect(renderTerm({ key: 'brand', value: '-acme' })).toBe('brand:"-acme"');
		expect(renderTerm({ key: 'brand', value: '-' })).toBe('brand:"-"');
		// And a leading star would be read as a wildcard rather than as the value it is.
		expect(renderTerm({ key: 'brand', value: '*acme' })).toBe('brand:"*acme"');
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

describe('the advanced form (Q.16)', () => {
	it('writes the shorthand each operator means', () => {
		expect(renderCondition({ key: 'brand', operator: 'is', value: 'acme' })).toBe('brand:acme');
		// Quoted, because `brand:Acme Corp` is a brand filter plus the free text "Corp".
		expect(renderCondition({ key: 'brand', operator: 'is', value: 'Acme Corp' })).toBe(
			'brand:"Acme Corp"'
		);
		expect(renderCondition({ key: 'brand', operator: 'is_not', value: 'acme' })).toBe(
			'NOT brand:acme'
		);
		// The star sits outside the quotes: quoting is what suppresses operator meaning, and a wildcard is one.
		expect(renderCondition({ key: 'brand', operator: 'contains', value: 'cme' })).toBe(
			'brand:*cme*'
		);
		expect(renderCondition({ key: 'brand', operator: 'starts_with', value: 'ac' })).toBe(
			'brand:ac*'
		);
		expect(renderCondition({ key: 'year', operator: 'at_least', value: '2024' })).toBe(
			'year:>=2024'
		);
		expect(renderCondition({ key: 'year', operator: 'at_most', value: '2026' })).toBe(
			'year:<=2026'
		);
		expect(
			renderCondition({ key: 'year', operator: 'between', value: '2024', upper: '2026' })
		).toBe('year:2024..2026');
		expect(renderCondition({ key: 'brand', operator: 'any', value: '' })).toBe('brand:*');
		expect(renderCondition({ key: 'brand', operator: 'empty', value: '' })).toBe('brand:-');
	});

	it('says nothing for a row somebody is still filling in', () => {
		// A preview that showed `brand:` while the user was mid-thought would report an error against their own
		// unfinished work.
		expect(renderCondition({ key: 'brand', operator: 'is', value: '' })).toBeNull();
		expect(renderCondition({ key: '', operator: 'is', value: 'acme' })).toBeNull();
		expect(renderCondition({ key: 'year', operator: 'between', value: '2024' })).toBeNull();
	});

	it('offers only the operators a kind can answer', () => {
		expect(operatorsFor('text')).toContain('contains');
		expect(operatorsFor('text')).not.toContain('between');
		expect(operatorsFor('int')).toContain('between');
		// A wildcard over a number has no meaning that is not an accident of formatting, and the server
		// refuses it — so the form does not offer it.
		expect(operatorsFor('int')).not.toContain('contains');
		expect(operatorsFor('bool')).toEqual(['is', 'any', 'empty']);
		// An unknown kind falls back to the text operators rather than to none: a newer schema should leave the
		// form usable rather than empty.
		expect(operatorsFor('colour-wheel')).toEqual(operatorsFor('text'));
	});

	it('parenthesises an OR join but not an AND', () => {
		const rows: Condition[] = [
			{ key: 'brand', operator: 'is', value: 'acme' },
			{ key: 'year', operator: 'at_least', value: '2024' }
		];
		// The word rather than juxtaposition: both parse as an AND, and a user reading the box should see the
		// join the form used rather than having to know that a space is one.
		expect(composeConditions(rows, 'AND')).toBe('brand:acme AND year:>=2024');
		// `a OR b c` binds differently than the form reads, and the result would be a query that looks like the
		// form and returns something else.
		expect(composeConditions(rows, 'OR')).toBe('(brand:acme OR year:>=2024)');
		expect(composeConditions([], 'AND')).toBe('');
		expect(composeConditions([{ key: 'brand', operator: 'is', value: 'acme' }], 'OR')).toBe(
			'brand:acme'
		);
	});

	it('turns a pasted list of filenames into one OR group', () => {
		expect(composeFilenames('a.jpg\nb.jpg')).toBe('(filename:a.jpg OR filename:b.jpg)');
		// A hyphenated name — the usual shape of a filename — reads as itself rather than as an escaped string.
		expect(composeFilenames('sample-003.jpg')).toBe('filename:sample-003.jpg');
		// Commas too: a list arrives from a spreadsheet column or from a sentence.
		expect(composeFilenames('a.jpg, b.jpg')).toBe('(filename:a.jpg OR filename:b.jpg)');
		expect(composeFilenames('  DSC_0043.jpg  ')).toBe('filename:DSC_0043.jpg');
		// A name with a space is quoted like any other value.
		expect(composeFilenames('my photo.jpg')).toBe('filename:"my photo.jpg"');
		expect(composeFilenames('\n\n')).toBe('');
	});

	it('narrows a query without changing what it already meant', () => {
		expect(narrow('brand:acme', 'year:2026')).toBe('brand:acme AND year:2026');
		// The existing query is grouped when it holds a top-level OR: narrowing `(a OR b)` with `c` has to mean
		// `(a OR b) AND c`, or the result is wider than what the user was looking at.
		expect(narrow('brand:acme OR brand:globex', 'year:2026')).toBe(
			'(brand:acme OR brand:globex) AND year:2026'
		);
		// Already parenthesised, it is left alone rather than wrapped twice.
		expect(narrow('(brand:acme OR brand:globex)', 'year:2026')).toBe(
			'(brand:acme OR brand:globex) AND year:2026'
		);
		expect(narrow('', 'year:2026')).toBe('year:2026');
		expect(narrow('brand:acme', '   ')).toBe('brand:acme');
	});
});
