/**
 * Composing the shorthand query string the filter rail and the search box share.
 *
 * The server parses one language (`dam_core::shorthand`); the rail's job is to write it. Doing that with
 * string concatenation at each call site is how a rail ends up emitting `brand:Acme Corp` — two terms, one
 * of which is free text — so quoting and de-duplication live here, with tests.
 *
 * ## The rail edits the query rather than filtering its results
 *
 * A rail that kept its selections separate from the text box would leave a user with two filters and no
 * way to see the whole thing, and the "copy this search" affordance would copy half of it. One string
 * means what the user sees is what the server got.
 */

/** One `key:value` filter. */
export type Term = {
	key: string;
	value: string;
};

/**
 * Whether a value needs quoting.
 *
 * Anything with whitespace or a character the parser treats as structure. Over-quoting is harmless — the
 * parser strips quotes — and under-quoting silently changes the query, so this errs towards quoting.
 */
function needsQuotes(value: string): boolean {
	return /[\s:"()>=<|-]/.test(value) || value.length === 0;
}

/** One term as the parser expects it. */
export function renderTerm(term: Term): string {
	const value = needsQuotes(term.value)
		? `"${term.value.replace(/(["\\])/g, '\\$1')}"`
		: term.value;
	return `${term.key}:${value}`;
}

/**
 * Splits a query into the terms this module manages and the free text around them.
 *
 * Deliberately conservative: anything it does not recognise as `key:value` is free text and is preserved
 * verbatim. A rail that dropped what it could not parse would silently discard a user's hand-typed range
 * the first time they clicked a facet.
 */
export function parse(query: string): { terms: Term[]; text: string } {
	const terms: Term[] = [];
	const rest: string[] = [];

	// Tokens are whitespace-separated except inside quotes, which is the only structure this needs to
	// respect in order to leave hand-written queries intact.
	const tokens = query.match(/(?:[^\s"]|"(?:\\.|[^"])*")+/g) ?? [];
	for (const token of tokens) {
		const match = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/.exec(token);
		if (!match) {
			rest.push(token);
			continue;
		}
		const [, key, raw] = match;
		// A comparison operator is a range or a negation, which this module does not own — left as text so
		// clicking a facet cannot rewrite `year:>2020` into something else.
		if (/^[<>=!]/.test(raw)) {
			rest.push(token);
			continue;
		}
		terms.push({ key, value: unquote(raw) });
	}

	return { terms, text: rest.join(' ') };
}

function unquote(value: string): string {
	if (!value.startsWith('"')) return value;
	return value.slice(1, value.endsWith('"') ? -1 : undefined).replace(/\\(["\\])/g, '$1');
}

/** The query with `term` added, or unchanged if it is already there. */
export function withTerm(query: string, term: Term): string {
	const { terms, text } = parse(query);
	if (terms.some((existing) => existing.key === term.key && existing.value === term.value)) {
		return query;
	}
	return compose([...terms, term], text);
}

/** The query with `term` removed. */
export function withoutTerm(query: string, term: Term): string {
	const { terms, text } = parse(query);
	return compose(
		terms.filter((existing) => !(existing.key === term.key && existing.value === term.value)),
		text
	);
}

/** Adds `term` if absent, removes it if present. What a facet checkbox does. */
export function toggleTerm(query: string, term: Term): string {
	const { terms } = parse(query);
	const present = terms.some(
		(existing) => existing.key === term.key && existing.value === term.value
	);
	return present ? withoutTerm(query, term) : withTerm(query, term);
}

/**
 * The query with exactly one term for `key` — `value` — or none when `value` is null.
 *
 * What a *browse tree* does, as against a facet checkbox. `in:a in:b` is a legal query and means "filed in
 * both", which is almost never what somebody clicking through a hierarchy wants: they are navigating, and two
 * branches selected at once usually returns nothing while looking like a bug in the tree. So selecting a
 * category replaces the previous one, and selecting the current one again clears it.
 */
export function withOnlyTerm(query: string, key: string, value: string | null): string {
	const { terms, text } = parse(query);
	const others = terms.filter((existing) => existing.key !== key);
	return compose(value === null ? others : [...others, { key, value }], text);
}

/** The value currently selected for `key`, if any. What decides which branch of a tree is highlighted. */
export function selectedValue(query: string, key: string): string | null {
	return parse(query).terms.find((term) => term.key === key)?.value ?? null;
}

/** Whether `term` is currently in `query`. What decides a checkbox's state. */
export function hasTerm(query: string, term: Term): boolean {
	return parse(query).terms.some(
		(existing) => existing.key === term.key && existing.value === term.value
	);
}

/**
 * Terms first, then free text.
 *
 * The order is for the reader rather than the parser: a user scanning the box sees their filters grouped
 * instead of interleaved with whatever they typed.
 */
function compose(terms: Term[], text: string): string {
	return [...terms.map(renderTerm), text].filter((part) => part.length > 0).join(' ');
}
