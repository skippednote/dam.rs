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
 * Whitespace and the characters the parser treats as structure. Under-quoting silently changes the query, so
 * this errs towards quoting — but not blindly: a hyphen *inside* a value is an ordinary character, and quoting
 * for it turned every pasted filename into `filename:"sample-003.jpg"`. That reads like an escape somebody
 * should worry about, in a box whose whole job is to be readable. A leading hyphen or a lone one is different:
 * `-` on its own is the "is empty" operator.
 */
function needsQuotes(value: string): boolean {
	if (value.length === 0) return true;
	if (/[\s:"()>=<|]/.test(value)) return true;
	// A leading `-` or `*` is an operator; anywhere else they are characters. `*` in particular has to stay
	// unquoted when the caller means it as a wildcard, which is why those callers build the term themselves.
	return /^[-*]/.test(value) || value === '-';
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

/**
 * The operators the advanced form offers, and the shorthand each one writes (Q.16).
 *
 * Named for what they ask rather than for the syntax they produce: somebody building a filter is thinking
 * "starts with", not `key:value*`. The mapping lives here with the rest of the query-writing so a second call
 * site cannot invent a different spelling for the same question.
 */
export type Operator =
	| 'is'
	| 'is_not'
	| 'contains'
	| 'starts_with'
	| 'at_least'
	| 'at_most'
	| 'between'
	| 'any'
	| 'empty';

/** Which operators make sense for a field of this kind. */
export function operatorsFor(kind: string): Operator[] {
	if (kind === 'bool') return ['is', 'any', 'empty'];
	if (kind === 'int' || kind === 'decimal' || kind === 'date' || kind === 'datetime') {
		return ['is', 'is_not', 'at_least', 'at_most', 'between', 'any', 'empty'];
	}
	// Text, and anything this build does not know: equality and matching are the operators every kind of text
	// answers, and offering a range over a name would produce a query the server refuses.
	return ['is', 'is_not', 'contains', 'starts_with', 'any', 'empty'];
}

/** One row of the advanced form. */
export type Condition = {
	key: string;
	operator: Operator;
	value: string;
	/** The second bound, for `between`. */
	upper?: string;
};

/**
 * One condition as the parser expects it, or `null` when it has nothing to say yet.
 *
 * `null` rather than a broken string: a half-filled row in a form is a row the user is still typing, and
 * emitting `brand:` for it would make the preview show an error against their own work in progress.
 */
export function renderCondition(condition: Condition): string | null {
	const { key, operator, value, upper } = condition;
	if (!key) return null;
	const needsValue = !['any', 'empty'].includes(operator);
	if (needsValue && value.trim() === '') return null;

	// The star has to sit *outside* the quotes or the parser reads it as part of the value — quoting is what
	// suppresses operator meaning, which is exactly what a wildcard is.
	const quoted = renderTerm({ key, value: value.trim() });
	const bare = value.trim();
	switch (operator) {
		case 'is':
			return quoted;
		case 'is_not':
			return `NOT ${quoted}`;
		case 'contains':
			return `${key}:*${bare}*`;
		case 'starts_with':
			return `${key}:${bare}*`;
		case 'at_least':
			return `${key}:>=${bare}`;
		case 'at_most':
			return `${key}:<=${bare}`;
		case 'between':
			return upper && upper.trim() !== '' ? `${key}:${bare}..${upper.trim()}` : null;
		case 'any':
			return `${key}:*`;
		case 'empty':
			return `${key}:-`;
	}
}

/**
 * The conditions as one query, joined by `AND` or `OR`.
 *
 * Parenthesised when there is more than one and the join is `OR`, because `a OR b c` binds differently than
 * anybody reading the form expects — and the result would be a query that looks like the form and returns
 * something else.
 */
export function composeConditions(conditions: Condition[], join: 'AND' | 'OR'): string {
	const parts = conditions.map(renderCondition).filter((part): part is string => part !== null);
	if (parts.length === 0) return '';
	if (parts.length === 1) return parts[0];
	const joined = parts.join(` ${join} `);
	return join === 'OR' ? `(${joined})` : joined;
}

/**
 * A pasted list of filenames as one query (Q.16's multiple-asset search).
 *
 * Splits on newlines and commas, because a list arrives from a spreadsheet column or from a sentence. Each
 * name is an exact `filename:` term and the group is an `OR`: somebody holding forty names wants the forty
 * assets, not the assets that somehow match all forty names at once.
 */
export function composeFilenames(pasted: string): string {
	const names = pasted
		.split(/[\n,]/)
		.map((name) => name.trim())
		.filter((name) => name.length > 0);
	if (names.length === 0) return '';
	const terms = names.map((name) => renderTerm({ key: 'filename', value: name }));
	return terms.length === 1 ? terms[0] : `(${terms.join(' OR ')})`;
}

/**
 * `query` narrowed by `addition` — the "search within these results" move.
 *
 * An `AND` of the two, with the existing query parenthesised when it contains a top-level `OR`: narrowing
 * `(a OR b)` with `c` has to mean `(a OR b) AND c` and not `a OR (b AND c)`, which returns *more* than the
 * user was looking at rather than less.
 */
export function narrow(query: string, addition: string): string {
	const existing = query.trim();
	const extra = addition.trim();
	if (extra === '') return existing;
	if (existing === '') return extra;
	const grouped =
		/\bOR\b/.test(existing) && !/^\(.*\)$/.test(existing) ? `(${existing})` : existing;
	return `${grouped} AND ${extra}`;
}

/**
 * The word being typed at the end of the query — what a type-ahead is about (Q.17).
 *
 * The *last* token, and only when the caret is at the end of it, because that is what a type-ahead means: a
 * suggestion for a word somebody has finished typing in the middle of a query would replace text they are not
 * looking at. Returns the empty string when the query ends in whitespace: they have moved on.
 */
export function trailingWord(query: string): string {
	// No guard for a trailing space or an empty query: splitting on whitespace leaves an empty final token in
	// exactly those cases, which is already "nothing to complete". A guard here would be a branch no test could
	// tell from its absence.
	const tokens = query.split(/\s+/);
	const last = tokens[tokens.length - 1] ?? '';
	// A `key:value` token being typed is a value, and that is the part to complete: somebody typing
	// `brand:acm` wants brands, not fields.
	const colon = last.lastIndexOf(':');
	return colon === -1 ? last : last.slice(colon + 1);
}

/**
 * `query` with its trailing token replaced by `fragment`.
 *
 * The whole token, not just the part that was typed: a suggestion is `brand:acme`, and appending it after a
 * half-typed `brand:acm` would leave two clauses where the user made one.
 */
export function completeWith(query: string, fragment: string): string {
	const trimmedEnd = query.replace(/\s+$/, '');
	if (trimmedEnd === '') return fragment;
	const tokens = trimmedEnd.split(/\s+/);
	if (/\s$/.test(query)) return `${trimmedEnd} ${fragment}`;
	tokens[tokens.length - 1] = fragment;
	return tokens.join(' ');
}

/**
 * `query` with the name at `column` replaced by `suggestion` — the one-click fix beside a parse refusal.
 *
 * `column` is the parser's own 1-based character position, and which half of a `key:value` token it points at
 * is the whole signal: the start of the token means the *key* was not recognised (`brnad:acme`), anywhere
 * further in means the *value* was not (`is:favourit`). Getting that wrong produces `brand:favourite` from
 * either, which is a correction nobody can make sense of.
 *
 * Returns the query unchanged when the column falls outside it, rather than guessing at a token.
 */
export function correctAt(query: string, column: number, suggestion: string): string {
	const index = column - 1;
	if (index < 0 || index >= query.length) return query;

	// Token boundaries around the column, by whitespace — the same unit the parser lexes.
	let start = index;
	while (start > 0 && !/\s/.test(query[start - 1])) start -= 1;
	let end = index;
	while (end < query.length && !/\s/.test(query[end])) end += 1;

	const token = query.slice(start, end);
	const colon = token.indexOf(':');
	// The column points at the key when it is at the token's start, or when the token has no `:` at all.
	const replacingKey = index === start || colon === -1;
	const replacement = replacingKey
		? colon === -1
			? suggestion
			: `${suggestion}${token.slice(colon)}`
		: `${token.slice(0, colon + 1)}${suggestion}`;
	return `${query.slice(0, start)}${replacement}${query.slice(end)}`;
}
