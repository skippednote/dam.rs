<!--
	The search box's type-ahead (Q.17).

	## Why this is a combobox and not a list of links

	A suggestion list that only works with a mouse is a search box that only works with a mouse: the box is
	where somebody's hands already are, and reaching for a pointer to accept a completion is slower than
	finishing the word. So this is the ARIA combobox pattern — the input keeps focus and keeps its value, the
	list is `aria-controls`-linked, and the active option travels by `aria-activedescendant` rather than by
	moving focus. Arrow keys move, Enter accepts, Escape closes without changing the query.

	## What it inserts

	The server's `fragment`, verbatim. A suggestion the client had to assemble into a filter would be a second
	place where the query language is spoken — and the place it would be got wrong is quoting, where the
	symptom is a query that changes when it is clicked.
-->
<script lang="ts">
	import { loadSuggestions, type Suggestion } from '$lib/api/client';
	import { completeWith, trailingWord } from '$lib/search/query';

	let {
		query,
		onquery
	}: {
		query: string;
		onquery: (next: string) => void;
	} = $props();

	let suggestions = $state<Suggestion[]>([]);
	let active = $state(-1);
	let open = $state(false);
	/** Rising counter, so a slow response for an older prefix cannot overwrite a newer one. */
	let generation = 0;

	/**
	 * How long to wait after a keystroke.
	 *
	 * Each call is three queries over the caller's visible library. A request per keystroke would send five for
	 * a five-letter word and show only the last one's answer — the other four are load nobody sees.
	 */
	const DEBOUNCE_MS = 150;
	let timer: ReturnType<typeof setTimeout> | undefined;

	export function close() {
		open = false;
		active = -1;
	}

	/** Called by the parent on input, because the parent owns the box. */
	export function typed() {
		const word = trailingWord(query);
		if (word.length < 2) {
			suggestions = [];
			close();
			return;
		}
		clearTimeout(timer);
		timer = setTimeout(() => void fetchFor(word), DEBOUNCE_MS);
	}

	async function fetchFor(word: string) {
		const mine = ++generation;
		try {
			// The rest of the query travels too, so the offer narrows as the search narrows. The word being
			// typed is removed from it: an unfinished clause would either fail to parse or filter the
			// suggestions down to the thing already typed.
			const context = completeWith(query, '').trim();
			const found = await loadSuggestions(word, context);
			if (mine !== generation) return;
			suggestions = found;
			open = found.length > 0;
			active = -1;
		} catch {
			// A type-ahead is an affordance. A failed suggestion request must not put an error where somebody
			// is mid-word — the box still works, and the results are the page.
			if (mine !== generation) return;
			suggestions = [];
			close();
		}
	}

	function accept(index: number) {
		const chosen = suggestions[index];
		if (!chosen) return;
		onquery(completeWith(query, chosen.fragment));
		suggestions = [];
		close();
	}

	/** Called by the parent from the input's `keydown`. Returns whether the key was used. */
	export function key(event: KeyboardEvent): boolean {
		if (!open || suggestions.length === 0) return false;
		if (event.key === 'ArrowDown') {
			active = (active + 1) % suggestions.length;
			return true;
		}
		if (event.key === 'ArrowUp') {
			active = active <= 0 ? suggestions.length - 1 : active - 1;
			return true;
		}
		if (event.key === 'Enter' && active >= 0) {
			accept(active);
			return true;
		}
		if (event.key === 'Escape') {
			close();
			return true;
		}
		return false;
	}

	/** The id of the active option, for the parent's `aria-activedescendant`. */
	export function activeId(): string | undefined {
		return active >= 0 ? `suggestion-${active}` : undefined;
	}

	export function isOpen(): boolean {
		return open;
	}

	/** A source's heading. The server sends a key; a heading is presentation. */
	function heading(source: string): string {
		if (source === 'filename') return 'Filenames';
		if (source === 'term') return 'Categories and tags';
		return 'Values';
	}
</script>

{#if open && suggestions.length > 0}
	<!--
		Positioned by the parent. `listbox` rather than `menu`: these are values to choose from, not commands,
		and a screen reader announces the difference.
	-->
	<ul
		id="search-suggestions"
		role="listbox"
		aria-label="Suggestions"
		class="absolute top-full right-0 left-0 z-20 mt-1 max-h-72 overflow-y-auto rounded-md border border-line bg-bg py-1 shadow-lg"
	>
		{#each suggestions as suggestion, index (suggestion.fragment)}
			<!--
				`aria-selected` on the active option, with focus staying in the box. Moving focus into the list
				would take the caret out of the query somebody is still typing.
			-->
			<li
				id={`suggestion-${index}`}
				role="option"
				aria-selected={index === active}
				class="flex cursor-pointer items-baseline gap-2 px-3 py-1.5 text-sm {index === active
					? 'bg-accent text-accent-fg'
					: ''}"
				onmousedown={(event) => {
					// `mousedown`, not `click`: the box loses focus first on a click, and a blur handler that
					// closed the list would eat the selection.
					event.preventDefault();
					accept(index);
				}}
			>
				<span class="flex-1 truncate">{suggestion.label}</span>
				<span class="text-xs {index === active ? 'text-accent-fg' : 'text-muted'}">
					{suggestion.within ?? heading(suggestion.source)}
				</span>
				{#if suggestion.count > 1}
					<span class="text-xs tabular-nums {index === active ? 'text-accent-fg' : 'text-muted'}">
						{suggestion.count}
					</span>
				{/if}
			</li>
		{/each}
	</ul>
{/if}
