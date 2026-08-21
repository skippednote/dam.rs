<!--
	Which filters the refine-search rail offers, and in what order (Q.19).

	## Why this is one list rather than a grid of checkboxes

	The rail *is* an order. A screen that only offered on/off would leave the arrangement to whatever the schema
	implied, which is the state this exists to fix: a library with thirty facetable fields has a rail nobody
	scrolls to the bottom of, and the two filters that matter are wherever `display_order` happened to put them.
	So the control is a list with move-up and move-down, and enabling is a checkbox on the row.

	## Move buttons rather than drag

	Drag-and-drop needs a keyboard equivalent to be usable at all, and the keyboard equivalent *is* move-up and
	move-down. Building the buttons first means one interaction that works everywhere; the pointer version is an
	enhancement somebody can add on top, not a prerequisite.

	## The disabled entries stay on screen

	Below a divider, greyed, with their checkboxes live. A screen that hid what it had switched off would be a
	screen where re-enabling a filter means guessing its name — and the four built-ins have no other home, so
	turning ratings off would make ratings unreachable.
-->
<script lang="ts">
	import { listRail, setRail, type RailEntry } from '$lib/api/client';
	import { onMount } from 'svelte';

	let entries = $state<RailEntry[]>([]);
	let error = $state('');
	let saving = $state(false);
	let saved = $state(false);

	/** What each kind is called on screen. The server sends a key; a heading is presentation. */
	const KINDS: Record<string, string> = {
		field: 'Metadata field',
		taxonomy: 'Vocabulary',
		builtin: 'Built in'
	};

	/** The built-ins' own names read better than their query selectors. `stars` is the selector; Rating is the word. */
	const BUILTIN_LABELS: Record<string, string> = {
		status: 'Status',
		orientation: 'Orientation',
		stars: 'Rating',
		has: 'Attachments'
	};

	function label(entry: RailEntry): string {
		if (entry.kind === 'builtin') {
			return BUILTIN_LABELS[entry.label] ?? entry.label;
		}
		return entry.label;
	}

	const enabled = $derived(entries.filter((entry) => entry.is_enabled));
	const disabled = $derived(entries.filter((entry) => !entry.is_enabled));

	async function load() {
		try {
			entries = await listRail();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not read the rail configuration.';
		}
	}

	onMount(load);

	/**
	 * Saves the enabled entries in their current order.
	 *
	 * The whole list every time, because the order is the value: two administrators each moving one row would
	 * otherwise produce an arrangement neither of them chose.
	 */
	async function save() {
		saving = true;
		saved = false;
		error = '';
		try {
			await setRail(enabled.map((entry) => entry.entry));
			// Re-read rather than assume: the server decides where a newly disabled entry lands, and a screen
			// that guessed would disagree with the rail the next time anybody looked.
			await load();
			saved = true;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not save the rail configuration.';
		} finally {
			saving = false;
		}
	}

	function move(entry: string, by: -1 | 1) {
		const order = entries.slice();
		const from = order.findIndex((one) => one.entry === entry);
		const to = from + by;
		if (from === -1 || to < 0 || to >= order.length) return;
		[order[from], order[to]] = [order[to], order[from]];
		entries = order;
		saved = false;
	}

	function toggle(entry: string) {
		entries = entries.map((one) =>
			one.entry === entry ? { ...one, is_enabled: !one.is_enabled } : one
		);
		saved = false;
	}
</script>

<section class="space-y-4" aria-label="Refine search">
	<div>
		<h2 class="text-sm font-semibold tracking-tight">Refine search</h2>
		<p class="mt-1 text-xs text-muted">
			The filters the rail offers, in order. A field has to be facetable before it can appear here;
			switching one off leaves it searchable in the box and takes it off the panel.
		</p>
	</div>

	{#if error}
		<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
	{/if}

	<ol class="space-y-1">
		{#each enabled as entry, index (entry.entry)}
			<li class="flex items-center gap-2 rounded-md border border-line px-2 py-1.5 text-sm">
				<input
					type="checkbox"
					class="rounded border-line text-accent focus:ring-accent"
					checked={true}
					aria-label={`Show ${label(entry)}`}
					onchange={() => toggle(entry.entry)}
				/>
				<span class="flex-1 truncate">{label(entry)}</span>
				<span class="text-xs text-muted">{KINDS[entry.kind] ?? entry.kind}</span>
				<button
					type="button"
					class="rounded px-1.5 text-xs disabled:opacity-30"
					disabled={index === 0}
					aria-label={`Move ${label(entry)} up`}
					onclick={() => move(entry.entry, -1)}
				>
					↑
				</button>
				<button
					type="button"
					class="rounded px-1.5 text-xs disabled:opacity-30"
					disabled={index === enabled.length - 1}
					aria-label={`Move ${label(entry)} down`}
					onclick={() => move(entry.entry, 1)}
				>
					↓
				</button>
			</li>
		{/each}
	</ol>

	{#if enabled.length === 0}
		<p class="text-xs text-muted">
			Nothing is enabled, so the rail is empty. Switch something on below.
		</p>
	{/if}

	{#if disabled.length > 0}
		<div class="space-y-1">
			<p class="text-xs font-semibold tracking-wide text-muted uppercase">Not shown</p>
			{#each disabled as entry (entry.entry)}
				<label
					class="flex items-center gap-2 rounded-md border border-dashed border-line px-2 py-1.5 text-sm text-muted"
				>
					<input
						type="checkbox"
						class="rounded border-line text-accent focus:ring-accent"
						checked={false}
						onchange={() => toggle(entry.entry)}
					/>
					<span class="flex-1 truncate">{label(entry)}</span>
					<span class="text-xs">{KINDS[entry.kind] ?? entry.kind}</span>
				</label>
			{/each}
		</div>
	{/if}

	<div class="flex items-center gap-3">
		<button
			type="button"
			class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
			disabled={saving}
			onclick={save}
		>
			{saving ? 'Saving…' : 'Save order'}
		</button>
		{#if saved}
			<p role="status" class="text-xs text-muted">Saved. The rail follows this order.</p>
		{/if}
	</div>
</section>
