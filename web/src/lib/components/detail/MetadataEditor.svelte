<!--
	The metadata editor.

	The server is the validator. This component's job is to send a patch and put the answer next to the
	field it belongs to — not to duplicate the rules. A client-side copy of `required`, `pattern` and
	`max_length` would be a second schema that drifts from the first, and the drift shows up as a form
	that refuses something the server would have accepted, or accepts something it then rejects.

	**But the field's *shape* is not a rule, and the editor does need it.** `multivalued` decides whether a
	value is a string or an array, and a client that guesses sends `"blue, red"` to a field that takes an
	array — the server refuses it with a message about delimiters that a user can do nothing with. That was
	a real bug here, found by editing a multivalued field in a real browser, and it is why the definitions
	are fetched rather than inferred from whatever keys happened to appear in the facets.

	**Errors are bound to fields with `aria-describedby` and `aria-invalid`.** An error rendered in a banner
	at the top of a form is an error a screen-reader user meets after they have already left the field that
	caused it — and, for a schema with twenty fields, one they then have to hunt for.
-->
<script lang="ts">
	import {
		ApiError,
		saveMetadata,
		type FieldDefinition,
		type ValidationProblem
	} from '$lib/api/client';

	let {
		assetId,
		values,
		fields,
		onsaved
	}: {
		assetId: string;
		/** The stored document. */
		values: Record<string, unknown>;
		/** The tenant's definitions, in display order. Empty means none are defined yet. */
		fields: FieldDefinition[];
		onsaved?: (values: Record<string, unknown>) => void;
	} = $props();

	/** The edited state, keyed by field. Strings, because that is what an input holds. */
	let draft = $state<Record<string, string>>({});
	/**
	 * The document the draft is diffed against.
	 *
	 * Its own state rather than the `values` prop, and that is what makes "Saved" stay on screen. Diffing
	 * against the prop meant a successful save updated the parent, the parent re-rendered with new values,
	 * and the seeding effect fired and cleared the confirmation — so the user saw it flash and vanish. The
	 * bug was invisible in a component test and only showed up driving a real browser.
	 */
	let base = $state<Record<string, unknown>>({});
	let problems = $state<ValidationProblem[]>([]);
	let saving = $state(false);
	let error = $state('');
	let saved = $state(false);
	/** Which asset the draft belongs to, so a re-render does not re-seed it. */
	let seededFor = $state('');

	/**
	 * Only what a person may edit.
	 *
	 * A read-only field is set by ingest or by a connector, so offering it produces a refusal the user can do
	 * nothing about — and the count is reported below rather than the fields silently vanishing.
	 */
	const editable = $derived(fields.filter((field) => !field.read_only));

	// Keyed on the asset, not on its values: carrying an unsaved edit across a selection change would
	// silently apply one asset's caption to another, and re-seeding on every value change would discard an
	// edit in progress the moment anything upstream refreshed.
	$effect(() => {
		if (seededFor === assetId) return;
		seededFor = assetId;
		seed(values);
		problems = [];
		error = '';
		saved = false;
	});

	// A definition that arrives after seeding — the list is fetched, so it can land second — still needs a
	// box, and one absent from the draft binds to `undefined` and renders the word "undefined".
	$effect(() => {
		for (const field of editable) {
			if (!(field.key in draft)) draft[field.key] = render(field, base[field.key]);
		}
	});

	function seed(document: Record<string, unknown>) {
		base = document;
		const next: Record<string, string> = {};
		for (const field of editable) {
			next[field.key] = render(field, document[field.key]);
		}
		draft = next;
	}

	/** A stored value as an input's text. */
	function render(field: FieldDefinition, value: unknown): string {
		if (value === null || value === undefined) return '';
		if (Array.isArray(value)) return value.map((item) => String(item)).join(', ');
		if (typeof value === 'string') return value;
		return JSON.stringify(value);
	}

	/**
	 * An input's text as the value to send.
	 *
	 * `null` for an emptied box, because the server distinguishes an absent key (leave alone) from a present
	 * null (clear) — and an emptied box means the second. A multivalued field splits on commas here rather
	 * than sending the delimited string, which is the whole point of knowing `multivalued`.
	 */
	function parse(field: FieldDefinition, text: string): unknown {
		const trimmed = text.trim();
		if (trimmed.length === 0) return null;
		if (!field.multivalued) return trimmed;
		const items = trimmed
			.split(',')
			.map((item) => item.trim())
			.filter((item) => item.length > 0);
		// A box holding only commas is a mistake rather than an instruction, so it clears — an empty array
		// would be a *value*, and not one anybody typed on purpose.
		return items.length > 0 ? items : null;
	}

	/** The patch: only what changed. */
	const patch = $derived.by(() => {
		const changes: Record<string, unknown> = {};
		for (const field of editable) {
			const before = render(field, base[field.key]);
			const after = draft[field.key] ?? '';
			if (before === after) continue;
			changes[field.key] = parse(field, after);
		}
		return changes;
	});

	const dirty = $derived(Object.keys(patch).length > 0);

	function problemFor(key: string): ValidationProblem | undefined {
		return problems.find((problem) => problem.key === key);
	}

	function describedBy(
		field: FieldDefinition,
		problem: ValidationProblem | undefined
	): string | undefined {
		const ids = [
			problem ? `e-${field.key}` : null,
			field.multivalued ? `h-${field.key}` : null
		].filter((id): id is string => id !== null);
		return ids.length > 0 ? ids.join(' ') : undefined;
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!dirty || saving) return;
		saving = true;
		problems = [];
		error = '';
		saved = false;
		try {
			const result = await saveMetadata(assetId, patch);
			// The *server's* document, not the draft: the validator normalises, and a date it reformatted or
			// a number it coerced has to be what the panel shows — otherwise the next read looks like an
			// unexplained change.
			const stored = result.values as Record<string, unknown>;
			seed(stored);
			saved = true;
			onsaved?.(stored);
		} catch (caught) {
			if (caught instanceof ApiError && caught.problems.length > 0) {
				problems = caught.problems;
			} else {
				error = caught instanceof Error ? caught.message : 'Could not save.';
			}
		} finally {
			saving = false;
		}
	}
</script>

{#if fields.length === 0}
	<p class="text-sm text-muted">
		This tenant has no metadata fields defined yet. Add them in the schema, and they appear here.
	</p>
{:else}
	<form class="space-y-4" onsubmit={save}>
		{#each editable as field (field.key)}
			{@const problem = problemFor(field.key)}
			<div class="space-y-1">
				<label
					class="block text-xs font-semibold tracking-wide text-muted uppercase"
					for={`f-${field.key}`}
				>
					{field.label}
					{#if field.required}
						<!-- The glyph is hidden and the word is not: "asterisk" read aloud is not information. -->
						<span aria-hidden="true">*</span>
						<span class="sr-only">(required)</span>
					{/if}
				</label>
				<input
					id={`f-${field.key}`}
					class="w-full rounded-md border border-state-neutral bg-bg px-2 py-1.5 text-sm"
					bind:value={draft[field.key]}
					aria-invalid={problem ? 'true' : undefined}
					aria-describedby={describedBy(field, problem)}
				/>
				{#if field.multivalued}
					<p id={`h-${field.key}`} class="text-xs text-muted">Separate values with commas.</p>
				{/if}
				{#if problem}
					<!--
						`role="alert"` so the message is announced when it appears, and next to the field so a
						keyboard user meets it where the mistake was.
					-->
					<p id={`e-${field.key}`} role="alert" class="text-xs text-state-rights-denied-fg">
						{problem.detail}
					</p>
				{/if}
			</div>
		{/each}

		{#if error}
			<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
		{/if}

		<div class="flex items-center gap-3">
			<button
				type="submit"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50"
				disabled={!dirty || saving}
			>
				{saving ? 'Saving…' : 'Save'}
			</button>
			{#if saved && !dirty}
				<!-- `polite`, not `assertive`: a success does not need to interrupt what is being read. -->
				<span aria-live="polite" class="text-xs text-muted">Saved</span>
			{/if}
			{#if dirty && !saving}
				<span class="text-xs text-muted">
					{Object.keys(patch).length} unsaved change{Object.keys(patch).length === 1 ? '' : 's'}
				</span>
			{/if}
		</div>

		{#if fields.length > editable.length}
			<p class="text-xs text-muted">
				{fields.length - editable.length} read-only field{fields.length - editable.length === 1
					? ''
					: 's'} not shown — set by ingest or a connector.
			</p>
		{/if}
	</form>
{/if}
