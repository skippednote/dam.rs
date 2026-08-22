<!--
	Auto-import mappings: what a file's own EXIF and XMP become in this tenant's fields.

	## The source is a picker, never a text box

	A mapping's left-hand side has to be a name the extractor actually produces. Typed from memory, `Exif.Artist`
	saves happily against a table that says nothing is wrong and then never fires on a single upload — so the list
	comes from the server, which reads it from the extractor's own tables.

	## Grouped by field, because that is what priority means

	Two sources can feed one field: `xmp.creator` where an editor filled it in, `exif.artist` where only the camera
	did. Only the first match applies, so the rows are shown under the field they write and in the order they are
	tried — a flat alphabetical list would hide the one thing an administrator needs to see, which is which rule
	wins.

	## Overwrite is presented as the consequence, not the flag

	"Replaces what is there" is what the switch does to a library somebody has curated. Naming it after the column
	would make the safe default look like an omission.
-->
<script lang="ts">
	import {
		ApiError,
		amendAutoImportMapping,
		createAutoImportMapping,
		listAutoImportMappings,
		listAutoImportSources,
		removeAutoImportMapping,
		type AutoImportMapping,
		type SchemaField
	} from '$lib/api/client';

	let { fields }: { fields: SchemaField[] } = $props();

	let mappings = $state<AutoImportMapping[]>([]);
	let sources = $state<string[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let adding = $state(false);
	let confirmingRemoval = $state<string | null>(null);
	let draft = $state({ source: '', field_key: '', priority: 0, overwrite: false });

	/** Fields an import may write to: read-only ones describe the file and the server refuses them anyway. */
	const writable = $derived(fields.filter((field) => !field.read_only));

	/**
	 * The mappings grouped by target field, each group in the order the server tries them.
	 *
	 * A sequential fold rather than a lookup, because the server already sorts by `(field_key, priority)`: rows
	 * for one field arrive together and in the order they will be tried, so grouping runs of them preserves that
	 * order exactly. Re-sorting here would let the display and the resolution disagree about which rule wins.
	 */
	const grouped = $derived.by(() => {
		const groups: [string, AutoImportMapping[]][] = [];
		for (const mapping of mappings) {
			const last = groups.at(-1);
			if (last?.[0] === mapping.field_key) last[1].push(mapping);
			else groups.push([mapping.field_key, [mapping]]);
		}
		return groups;
	});

	function labelFor(key: string): string {
		return fields.find((field) => field.key === key)?.label ?? key;
	}

	/**
	 * Loads the mappings and the source list, settling each on its own.
	 *
	 * Not `Promise.all`: the two answers come from different places — the mappings from this tenant's table, the
	 * sources from the extractor, which cannot fail for any tenant reason. Failing them together meant that one
	 * bad list read also emptied the picker, so the form opened with nothing to choose and a `required` select
	 * that could never be satisfied. Whichever half arrives is shown.
	 */
	async function load() {
		error = '';
		const [listed, available] = await Promise.allSettled([
			listAutoImportMappings(),
			listAutoImportSources()
		]);
		if (listed.status === 'fulfilled') mappings = listed.value;
		if (available.status === 'fulfilled') sources = available.value;

		const failure = [listed, available].find((result) => result.status === 'rejected');
		if (failure?.status === 'rejected') {
			error =
				failure.reason instanceof ApiError
					? failure.reason.message
					: 'Could not load the mappings.';
		}
	}

	/** Runs an edit, keeps the server's sentence, reloads. */
	async function run(work: () => Promise<string>) {
		busy = true;
		error = '';
		try {
			notice = await work();
			await load();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'That change could not be made.';
		} finally {
			busy = false;
		}
	}

	function add(event: SubmitEvent) {
		event.preventDefault();
		void run(async () => {
			const created = await createAutoImportMapping({
				source: draft.source,
				field_key: draft.field_key,
				priority: Number(draft.priority) || 0,
				overwrite: draft.overwrite
			});
			adding = false;
			draft = { source: '', field_key: '', priority: 0, overwrite: false };
			return `${created.source} now fills ${labelFor(created.field_key)} on upload.`;
		});
	}

	function toggle(mapping: AutoImportMapping, change: { overwrite?: boolean; enabled?: boolean }) {
		void run(async () => {
			const amended = await amendAutoImportMapping(mapping.id, change);
			if (change.enabled !== undefined) {
				return amended.enabled
					? `${amended.source} will be imported again.`
					: `${amended.source} is switched off; nothing else changes.`;
			}
			return amended.overwrite
				? `${amended.source} will now replace a value that is already there.`
				: `${amended.source} will only fill ${labelFor(amended.field_key)} when it is empty.`;
		});
	}

	function remove(mapping: AutoImportMapping) {
		confirmingRemoval = null;
		void run(async () => {
			await removeAutoImportMapping(mapping.id);
			return `${mapping.source} no longer fills ${labelFor(mapping.field_key)}. Values it already imported stay on their assets.`;
		});
	}

	$effect(() => {
		void load();
	});
</script>

<section class="space-y-3" aria-label="Auto-import from embedded metadata">
	<div class="flex flex-wrap items-end justify-between gap-3">
		<div>
			<h2 class="text-lg font-semibold tracking-tight">Auto-import from the file</h2>
			<p class="mt-1 max-w-2xl text-sm text-muted">
				Cameras and editors write metadata inside the file — who took it, what it is of, what the
				exposure was. A mapping says which of your fields each of those fills, and it runs as the
				upload lands.
			</p>
		</div>
		<button
			type="button"
			class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised"
			onclick={() => (adding = !adding)}
			aria-expanded={adding}
			disabled={writable.length === 0 || sources.length === 0}
		>
			Add a mapping
		</button>
	</div>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-3 text-sm text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}
	{#if notice}
		<p role="status" class="rounded-md bg-surface p-3 text-sm">{notice}</p>
	{/if}

	{#if writable.length === 0}
		<p class="text-sm text-muted">
			There is nothing to import into yet. Define a field above first — an import writes the
			tenant's own fields, and the read-only ones describe the file rather than what you know about
			it.
		</p>
	{:else if sources.length === 0}
		<p class="text-sm text-muted">
			The list of things a file can carry could not be read, so there is nothing to choose from. A
			mapping has to name a source the server actually produces — offering a blank picker would only
			let you save a rule that never fires.
		</p>
	{:else if adding}
		<form class="flex flex-wrap items-end gap-3 rounded-md bg-surface p-4" onsubmit={add}>
			<label class="flex flex-col gap-1 text-sm">
				<span class="text-xs font-medium text-muted">In the file</span>
				<select
					class="w-52 rounded-md border border-line bg-bg px-2 py-1 font-mono text-sm"
					bind:value={draft.source}
					required
				>
					<option value="" disabled>Choose a source</option>
					{#each sources as source (source)}
						<option value={source}>{source}</option>
					{/each}
				</select>
			</label>
			<label class="flex flex-col gap-1 text-sm">
				<span class="text-xs font-medium text-muted">Fills</span>
				<select
					class="w-48 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={draft.field_key}
					required
				>
					<option value="" disabled>Choose a field</option>
					{#each writable as field (field.key)}
						<option value={field.key}>{field.label}</option>
					{/each}
				</select>
			</label>
			<label class="flex flex-col gap-1 text-sm">
				<span class="text-xs font-medium text-muted">Tried at</span>
				<input
					type="number"
					class="w-20 rounded-md border border-line bg-bg px-2 py-1 text-sm tabular-nums"
					bind:value={draft.priority}
				/>
			</label>
			<label class="flex items-center gap-2 pb-1 text-sm">
				<input type="checkbox" bind:checked={draft.overwrite} />
				Replace what is already there
			</label>
			<button
				type="submit"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
				disabled={busy || !draft.source || !draft.field_key}
			>
				Add
			</button>
			<button type="button" class="pb-1 text-xs underline" onclick={() => (adding = false)}>
				Cancel
			</button>
			<p class="w-full text-xs text-muted">
				Lower is tried first, so a second mapping onto the same field is the fallback for files that
				do not carry the first one. Leaving "replace" off means an import only ever fills an empty
				field — which is what keeps a re-import from undoing somebody's corrections.
			</p>
		</form>
	{/if}

	{#if mappings.length === 0}
		<p class="text-sm text-muted">
			Nothing is imported yet. Uploads keep their embedded metadata in the file either way; a
			mapping is what brings it into a field you can search and edit.
		</p>
	{:else}
		<ul class="space-y-3">
			{#each grouped as [fieldKey, group] (fieldKey)}
				<li class="rounded-md border border-line">
					<h3 class="border-b border-line px-3 py-2 text-sm font-medium">
						{labelFor(fieldKey)}
						{#if group.length > 1}
							<span class="ml-2 text-xs font-normal text-muted">
								{group.filter((m) => m.enabled).length} of {group.length} in use, first match wins
							</span>
						{/if}
					</h3>
					<ul>
						{#each group as mapping (mapping.id)}
							<li
								class="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-line px-3 py-2 last:border-b-0"
								class:bg-surface={!mapping.enabled}
							>
								<span class="font-mono text-sm">{mapping.source}</span>
								{#if !mapping.enabled}
									<span class="rounded bg-raised px-1.5 py-0.5 text-xs text-muted">off</span>
								{/if}
								{#if mapping.overwrite}
									<span
										class="rounded bg-state-rights-denied/18 px-1.5 py-0.5 text-xs font-medium text-state-rights-denied-fg"
									>
										replaces
									</span>
								{/if}
								<span class="text-xs text-muted tabular-nums">tried at {mapping.priority}</span>
								<span class="ml-auto flex items-center gap-3">
									<label class="flex items-center gap-1.5 text-xs">
										<input
											type="checkbox"
											checked={mapping.enabled}
											disabled={busy}
											onchange={(event) =>
												toggle(mapping, { enabled: event.currentTarget.checked })}
										/>
										On
									</label>
									<label class="flex items-center gap-1.5 text-xs">
										<input
											type="checkbox"
											checked={mapping.overwrite}
											disabled={busy}
											onchange={(event) =>
												toggle(mapping, { overwrite: event.currentTarget.checked })}
										/>
										Replace
									</label>
									<button
										type="button"
										class="text-xs underline"
										onclick={() =>
											(confirmingRemoval = confirmingRemoval === mapping.id ? null : mapping.id)}
									>
										Remove
									</button>
								</span>
								{#if confirmingRemoval === mapping.id}
									<div class="w-full rounded-md bg-surface p-3 text-xs">
										<p>
											Removing this stops future uploads from filling {labelFor(mapping.field_key)}
											from {mapping.source}. Values already imported stay on their assets — this is
											a decision about what happens next, not a way to undo it.
										</p>
										<span class="mt-2 flex items-center gap-2">
											<button
												type="button"
												class="rounded-md bg-state-rights-denied px-2.5 py-1 font-medium text-state-rights-denied-fg disabled:opacity-50"
												disabled={busy}
												onclick={() => remove(mapping)}
											>
												Remove {mapping.source}
											</button>
											<button
												type="button"
												class="underline"
												onclick={() => (confirmingRemoval = null)}
											>
												Cancel
											</button>
										</span>
									</div>
								{/if}
							</li>
						{/each}
					</ul>
				</li>
			{/each}
		</ul>
		<p class="text-xs text-muted">
			A value that does not fit its field is reported rather than stored — an exposure reading will
			not go into a whole-number field, and a timestamp needs the camera to have recorded its time
			zone before a date-and-time field will take it.
		</p>
	{/if}
</section>
