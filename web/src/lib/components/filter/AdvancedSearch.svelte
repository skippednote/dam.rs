<!--
	Advanced search (Q.16): a form that writes the query string, rather than a second way to search.

	Everything here composes shorthand and hands it back. That is the whole design: the box holds one query,
	the rail edits it, the browse tree edits it, and this form edits it too — so "copy this search" copies all
	of it and a user can always see what they asked for. A form that posted its own structured payload would
	be a second query language with its own bugs, and the box beside it would stop telling the truth.

	Three things live here, because they are the same act at different scales:

	- **Conditions** — field, operator, value, joined by AND or OR. The operators are named for what they ask
	  ("starts with") rather than for the syntax they produce (`key:value*`), and each kind is offered only the
	  ones it can answer: a range over a name is a query the server refuses, so the form does not suggest it.
	- **Filenames** — a pasted list becomes one OR group over `filename:`. Somebody arriving with forty names
	  off a delivery note wants the forty assets, and typing forty terms by hand is how that person gives up.
	- **Narrow** — the same composition ANDed onto whatever the box already holds, which is "search within
	  these results" without a second result set to keep in step.

	The preview is not decoration. A user about to replace their query should see what it will become, and it
	is also the fastest way to learn the language: the form is a teacher for the box.
-->
<script lang="ts">
	import {
		composeConditions,
		composeFilenames,
		narrow,
		operatorsFor,
		type Condition,
		type Operator
	} from '$lib/search/query';
	import type { FieldDefinition } from '$lib/api/client';

	let {
		fields,
		query,
		onquery,
		onclose
	}: {
		fields: FieldDefinition[];
		query: string;
		onquery: (next: string) => void;
		onclose: () => void;
	} = $props();

	/** The label for each operator. What the user picks from. */
	const LABELS: Record<Operator, string> = {
		is: 'is',
		is_not: 'is not',
		contains: 'contains',
		starts_with: 'starts with',
		at_least: 'is at least',
		at_most: 'is at most',
		between: 'is between',
		any: 'has any value',
		empty: 'is empty'
	};

	/**
	 * `filename` sits in the list beside the tenant's own fields.
	 *
	 * It is a column rather than a field definition, but to somebody building a filter that distinction is
	 * invisible and irrelevant — and leaving it out would mean the one thing every asset has is the one thing
	 * this form cannot filter by.
	 */
	const FILENAME: FieldDefinition = {
		key: 'filename',
		label: 'Filename',
		kind: 'text',
		multivalued: false,
		required: true,
		facetable: false,
		read_only: true,
		ai_writable: false
	};

	const choices = $derived([FILENAME, ...fields]);

	let rows = $state<Condition[]>([{ key: 'filename', operator: 'contains', value: '' }]);
	let join = $state<'AND' | 'OR'>('AND');
	let pasted = $state('');

	function kindOf(key: string): string {
		return choices.find((field) => field.key === key)?.kind ?? 'text';
	}

	/**
	 * The operator is reset when the field changes to one that cannot answer it.
	 *
	 * Without this, switching from a year to a brand leaves "is between" selected and the preview goes blank
	 * with no explanation — the row looks filled in and produces nothing.
	 */
	function fieldChanged(index: number, key: string) {
		const allowed = operatorsFor(kindOf(key));
		const current = rows[index].operator;
		rows[index] = {
			...rows[index],
			key,
			operator: allowed.includes(current) ? current : allowed[0]
		};
	}

	const composed = $derived.by(() => {
		const conditions = composeConditions(rows, join);
		const names = composeFilenames(pasted);
		// A list of names is an OR group of its own, ANDed with the conditions: "these forty assets, and only
		// the archived ones" is the question somebody with a list and a filter is asking.
		return [conditions, names].filter((part) => part !== '').join(' AND ');
	});

	const preview = $derived(composed === '' ? '' : composed);
	const narrowed = $derived(composed === '' ? query : narrow(query, composed));

	function addRow() {
		rows = [...rows, { key: choices[0]?.key ?? '', operator: 'is', value: '' }];
	}

	function removeRow(index: number) {
		rows = rows.filter((_, at) => at !== index);
	}

	/** Two lines, so the shape of the paste is visible before anybody pastes anything. */
	const NAMES_PLACEHOLDER = 'DSC_0043.jpg\nDSC_0044.jpg';
</script>

<!--
	A labelled region rather than a modal dialog: this panel edits the box above it, and trapping focus away
	from that box would hide the thing being edited. Escape still closes it, which is the part of a dialog's
	behaviour that matters here — bound to the window, because the panel only exists while it is open and a
	key listener on a `<section>` is a listener on something nobody can focus.
-->
<svelte:window
	onkeydown={(event) => {
		if (event.key === 'Escape') onclose();
	}}
/>

<section class="space-y-4 border-b border-line px-4 py-3" aria-labelledby="advanced-heading">
	<div class="flex items-center justify-between">
		<h2 id="advanced-heading" class="text-sm font-semibold tracking-tight">Advanced search</h2>
		<button type="button" class="text-xs text-muted underline" onclick={onclose}>Close</button>
	</div>

	<div class="space-y-2">
		{#each rows as row, index (index)}
			<div class="flex flex-wrap items-center gap-2">
				{#if index > 0}
					<label class="sr-only" for={`join-${index}`}>Join</label>
					<select
						id={`join-${index}`}
						class="rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
						bind:value={join}
					>
						<option value="AND">and</option>
						<option value="OR">or</option>
					</select>
				{:else}
					<span class="w-12 text-xs text-muted">Where</span>
				{/if}

				<label class="sr-only" for={`field-${index}`}>Field</label>
				<select
					id={`field-${index}`}
					class="rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
					value={row.key}
					onchange={(event) => fieldChanged(index, event.currentTarget.value)}
				>
					{#each choices as field (field.key)}
						<option value={field.key}>{field.label}</option>
					{/each}
				</select>

				<label class="sr-only" for={`operator-${index}`}>Operator</label>
				<select
					id={`operator-${index}`}
					class="rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
					bind:value={row.operator}
				>
					{#each operatorsFor(kindOf(row.key)) as operator (operator)}
						<option value={operator}>{LABELS[operator]}</option>
					{/each}
				</select>

				{#if !['any', 'empty'].includes(row.operator)}
					<label class="sr-only" for={`value-${index}`}>Value</label>
					<input
						id={`value-${index}`}
						class="min-w-32 flex-1 rounded-md border border-line bg-bg px-3 py-1.5 text-sm"
						bind:value={row.value}
					/>
				{/if}
				{#if row.operator === 'between'}
					<label class="sr-only" for={`upper-${index}`}>Upper bound</label>
					<input
						id={`upper-${index}`}
						class="min-w-32 flex-1 rounded-md border border-line bg-bg px-3 py-1.5 text-sm"
						placeholder="and"
						bind:value={row.upper}
					/>
				{/if}

				{#if rows.length > 1}
					<button
						type="button"
						class="text-xs text-state-rights-denied-fg underline"
						onclick={() => removeRow(index)}
					>
						Remove
					</button>
				{/if}
			</div>
		{/each}
		<button type="button" class="text-xs underline" onclick={addRow}>Add a condition</button>
	</div>

	<div>
		<label class="block text-xs font-semibold tracking-wide text-muted uppercase" for="filenames">
			Many assets at once
		</label>
		<p class="mt-1 text-xs text-muted">
			One filename per line, or separated by commas. Each is matched exactly, and the list is an
			<code class="font-mono">OR</code> — forty names find forty assets.
		</p>
		<textarea
			id="filenames"
			rows="3"
			class="mt-1 w-full rounded-md border border-line bg-bg px-3 py-1.5 font-mono text-sm"
			bind:value={pasted}
			placeholder={NAMES_PLACEHOLDER}></textarea>
	</div>

	<div class="space-y-2">
		<p class="text-xs text-muted">
			This writes:
			<code class="font-mono text-fg">{preview || '(nothing yet)'}</code>
		</p>
		<div class="flex flex-wrap items-center gap-2">
			<button
				type="button"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
				disabled={composed === ''}
				onclick={() => {
					onquery(composed);
					onclose();
				}}
			>
				Search
			</button>
			<!--
				"Search within" is this, and nothing more: an AND onto what the box already holds. A separate
				result set to narrow would be a second thing to keep in step with the first, and the URL would
				stop describing the page.
			-->
			<button
				type="button"
				class="rounded-md border border-line px-3 py-1.5 text-sm disabled:opacity-50"
				disabled={composed === '' || query.trim() === ''}
				onclick={() => {
					onquery(narrowed);
					onclose();
				}}
				title="Add these conditions to the current search"
			>
				Search within results
			</button>
		</div>
	</div>
</section>
