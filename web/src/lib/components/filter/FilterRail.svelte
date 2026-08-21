<!--
	The filter rail: facet buckets as checkboxes, writing the same query string the search box holds.

	Two decisions worth stating.

	**It edits the query, it does not filter results.** A rail that kept its own state beside the text box
	would leave a user with two filters and no way to see the whole thing — and "copy this search" would
	copy half of it. One string means what the user sees is what the server got.

	**`truncated` is rendered.** The server caps buckets, and a rail that silently cut off makes "no other
	brands" and "ninety other brands" look identical. A user filters on the wrong assumption and never
	learns why the result set was smaller than they expected.

	Native checkboxes rather than a bits-ui primitive: a checkbox is one of the few controls the platform
	already gets right — label association, indeterminate state, keyboard, screen-reader announcement — and
	wrapping it would be replacing working semantics with hand-written ones.
-->
<script lang="ts">
	import type { Facet } from '$lib/api/client';
	import { hasTerm, toggleTerm } from '$lib/search/query';

	let {
		facets,
		query,
		onquery
	}: {
		facets: Facet[];
		query: string;
		onquery: (next: string) => void;
	} = $props();

	// Empty facets are dropped rather than rendered as an empty group: a heading with nothing under it
	// reads as a loading state that never finishes.
	const populated = $derived(facets.filter((facet) => facet.buckets.length > 0));

	/**
	 * Headings and bucket text for the built-in facets (Q.15).
	 *
	 * The server sends a facet key that doubles as the query selector — `stars`, `has` — because the rail
	 * writes the string it reads. Neither is a heading anybody would write, and `has: attachment (12)` reads
	 * like a debug dump, so both are named here. Presentation, like every other label in this component.
	 */
	const BUILTIN_HEADINGS: Record<string, string> = {
		status: 'Status',
		orientation: 'Orientation',
		stars: 'Rating',
		has: 'Attachments'
	};

	function label(key: string): string {
		// The server sends the field key. Rendering `content_type` as "Content type" is presentation, and
		// it belongs here rather than in the wire type — a tenant's own label is a schema-admin concern.
		return BUILTIN_HEADINGS[key] ?? key.replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase());
	}

	/** A bucket's text. Only the built-ins need one; a metadata value is its own label. */
	function bucketText(key: string, value: string): string {
		if (key === 'stars') {
			// "4 stars", not "4": the count beside it is a number too, and two bare numbers in a row read as a
			// range. Singular for one, because "1 stars" is the kind of detail that makes a page feel unfinished.
			return value === '1' ? '1 star' : `${value} stars`;
		}
		if (key === 'has') return value === 'attachment' ? 'Has attachments' : value;
		if (key === 'status' || key === 'orientation') {
			return value.replace(/^./, (c) => c.toUpperCase());
		}
		return value;
	}
</script>

<div class="space-y-6">
	{#if populated.length === 0}
		<p class="text-sm text-muted">
			No facets yet. A field becomes facetable in the schema, and taxonomies appear once terms are
			applied.
		</p>
	{/if}

	{#each populated as facet (facet.key)}
		<!--
			`fieldset`/`legend` rather than a div and a heading: this is a group of related controls, and the
			legend is what a screen reader announces before each checkbox in it. Without it every box is
			announced as its bare value — "blue", "red" — with no clue which field they belong to.
		-->
		<fieldset class="space-y-1">
			<legend class="mb-1 text-xs font-semibold tracking-wide text-muted uppercase">
				{label(facet.key)}
			</legend>

			{#each facet.buckets as bucket (bucket.value)}
				{@const term = { key: facet.key, value: bucket.value }}
				{@const checked = hasTerm(query, term)}
				<label class="flex cursor-pointer items-center gap-2 py-0.5 text-sm">
					<input
						type="checkbox"
						class="rounded border-line text-accent focus:ring-accent"
						{checked}
						onchange={() => onquery(toggleTerm(query, term))}
					/>
					<span class="flex-1 truncate" title={bucket.value}
						>{bucketText(facet.key, bucket.value)}</span
					>
					<!--
						`tabular-nums` so the counts line up as a column; without it the digits jitter and the
						rail reads as ragged even though every row is aligned.
					-->
					<span class="text-xs text-muted tabular-nums">{bucket.count}</span>
				</label>
			{/each}

			{#if facet.truncated}
				<p class="pt-1 text-xs text-muted">
					Showing the {facet.buckets.length} most common. Others exist — type
					<code class="font-mono">{facet.key}:</code> in the search box to filter by one directly.
				</p>
			{/if}
		</fieldset>
	{/each}
</div>
