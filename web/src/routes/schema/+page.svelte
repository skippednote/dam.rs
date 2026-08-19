<script lang="ts">
	/**
	 * Schema administration: the tenant's metadata fields, and the edits that are safe to make.
	 *
	 * ## Every row leads with its usage count
	 *
	 * A field definition is what the validator refuses writes against, what the facet rail enumerates, and
	 * what every metadata form is built from — so the question behind every edit here is "how much data is
	 * already shaped like this". The count answers it in place, rather than making an administrator go and
	 * find out, and it is why the destructive actions can be honest about their consequences.
	 *
	 * ## The server's refusals are shown verbatim
	 *
	 * "brand cannot change kind: 2 asset(s) already carry a value" is a better sentence than anything this
	 * page could compose from a status code, and it carries the number that makes the refusal actionable.
	 */
	import { onMount } from 'svelte';
	import AutoImport from '$lib/components/schema/AutoImport.svelte';
	import MetadataTypes from '$lib/components/schema/MetadataTypes.svelte';
	import UploadProfiles from '$lib/components/schema/UploadProfiles.svelte';
	import {
		ApiError,
		amendField,
		defineField,
		listSchemaFields,
		removeField,
		reorderFields,
		type SchemaField
	} from '$lib/api/client';

	/** The kinds the validator knows, in the order an administrator is likely to want them. */
	const KINDS = [
		['text', 'Text'],
		['textarea', 'Text (multi-line)'],
		['long_text', 'Long text'],
		['int', 'Whole number'],
		['decimal', 'Decimal'],
		['bool', 'Yes / no'],
		['date', 'Date'],
		['datetime', 'Date and time'],
		['select', 'One of a list'],
		['multiselect', 'Several of a list'],
		['taxonomy_ref', 'Taxonomy term'],
		['user_ref', 'Person'],
		['url', 'URL'],
		['geo', 'Location']
	] as const;

	let fields = $state<SchemaField[]>([]);
	let error = $state('');
	/** The last thing that happened, so a consequence stays on screen after the list reloads. */
	let notice = $state('');
	let busy = $state(false);
	let adding = $state(false);
	let confirmingRemoval = $state<string | null>(null);

	let draft = $state({ key: '', label: '', kind: 'text', facetable: false, multivalued: false });

	async function load() {
		error = '';
		try {
			fields = await listSchemaFields();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load the schema.';
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
			const created = await defineField({
				key: draft.key.trim(),
				label: draft.label.trim() || draft.key.trim(),
				kind: draft.kind,
				facetable: draft.facetable,
				multivalued: draft.multivalued
			});
			adding = false;
			draft = { key: '', label: '', kind: 'text', facetable: false, multivalued: false };
			return created.assets_with_values > 0
				? `Added ${created.key}. ${created.assets_with_values} asset(s) already carry values under this key, from a previous definition — they are visible again.`
				: `Added ${created.key}.`;
		});
	}

	function amend(key: string, change: Record<string, unknown>) {
		void run(async () => {
			const amended = await amendField(key, change);
			const notes: string[] = [];
			if (amended.assets_now_incomplete > 0) {
				notes.push(
					`${amended.assets_now_incomplete} asset(s) have no value for it, so their next metadata save will be refused until one is set.`
				);
			}
			if (amended.reindex_required) {
				notes.push('Search and facets are stale until the index is rebuilt.');
			}
			return [`Updated ${key}.`, ...notes].join(' ');
		});
	}

	function remove(key: string) {
		void run(async () => {
			const removed = await removeField(key);
			confirmingRemoval = null;
			const kept =
				removed.assets_with_values > 0
					? ` The values on ${removed.assets_with_values} asset(s) are kept — re-adding ${key} as the same kind brings them back.`
					: '';
			const stale = removed.reindex_required
				? ' Search and facets are stale until the index is rebuilt.'
				: '';
			return `Removed ${key}.${kept}${stale}`;
		});
	}

	function move(index: number, delta: number) {
		const next = index + delta;
		if (next < 0 || next >= fields.length) return;
		const keys = fields.map((field) => field.key);
		[keys[index], keys[next]] = [keys[next], keys[index]];
		void run(async () => {
			await reorderFields(keys);
			return 'Field order saved.';
		});
	}

	function kindLabel(kind: string): string {
		return KINDS.find(([value]) => value === kind)?.[1] ?? kind;
	}

	onMount(load);
</script>

<div class="mx-auto max-w-5xl space-y-4 p-8">
	<!--
		Named, like the two sections below it: three landmarks on one page, and a screen-reader user should be
		able to jump between "the fields", "which forms use them" and "what each intake presumes" rather than
		walking the whole thing. It also keeps controls whose labels repeat across sections addressable.
	-->
	<section class="space-y-4" aria-label="Metadata fields">
		<div class="flex flex-wrap items-end justify-between gap-3">
			<div>
				<h1 class="text-2xl font-semibold tracking-tight">Metadata schema</h1>
				<p class="mt-1 max-w-2xl text-sm text-muted">
					These fields are what every form, filter and search in this tenant is built from. The
					count beside each one is how many assets already carry a value for it.
				</p>
			</div>
			<button
				type="button"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg"
				onclick={() => (adding = !adding)}
				aria-expanded={adding}
			>
				Add a field
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

		{#if adding}
			<form class="flex flex-wrap items-end gap-3 rounded-md bg-surface p-4" onsubmit={add}>
				<label class="flex flex-col gap-1 text-sm">
					<span class="text-xs font-medium text-muted">Key</span>
					<input
						class="w-40 rounded-md border border-line bg-bg px-2 py-1 font-mono text-sm"
						bind:value={draft.key}
						placeholder="campaign"
						required
					/>
				</label>
				<label class="flex flex-col gap-1 text-sm">
					<span class="text-xs font-medium text-muted">Label</span>
					<input
						class="w-48 rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={draft.label}
						placeholder="Campaign"
					/>
				</label>
				<label class="flex flex-col gap-1 text-sm">
					<span class="text-xs font-medium text-muted">Kind</span>
					<select
						class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={draft.kind}
					>
						{#each KINDS as [value, label] (value)}
							<option {value}>{label}</option>
						{/each}
					</select>
				</label>
				<label class="flex items-center gap-2 pb-1 text-sm">
					<input
						type="checkbox"
						class="rounded border-line text-accent"
						bind:checked={draft.multivalued}
					/>
					<span>Several values</span>
				</label>
				<label class="flex items-center gap-2 pb-1 text-sm">
					<input
						type="checkbox"
						class="rounded border-line text-accent"
						bind:checked={draft.facetable}
					/>
					<span>Filterable</span>
				</label>
				<button
					type="submit"
					class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
					disabled={busy || !draft.key.trim()}
				>
					Add
				</button>
				<button type="button" class="pb-1 text-xs underline" onclick={() => (adding = false)}>
					Cancel
				</button>
				<p class="w-full text-xs text-muted">
					A key is permanent: it is the name every stored value sits under, so renaming it later
					means adding a new field and moving the data. Lower-case letters, digits and underscores.
				</p>
			</form>
		{/if}

		{#if fields.length === 0 && !error}
			<p class="text-sm text-muted">No fields are defined yet.</p>
		{:else}
			<div class="overflow-x-auto rounded-md border border-line">
				<table class="w-full text-sm">
					<thead>
						<tr class="border-b border-line text-left text-xs tracking-wide text-muted uppercase">
							<th class="px-3 py-2 font-semibold">Field</th>
							<th class="px-3 py-2 font-semibold">Kind</th>
							<th class="px-3 py-2 font-semibold">Behaviour</th>
							<th class="px-3 py-2 font-semibold">In use</th>
							<th class="px-3 py-2 font-semibold">Order</th>
							<th class="px-3 py-2"><span class="sr-only">Actions</span></th>
						</tr>
					</thead>
					<tbody>
						{#each fields as field, index (field.key)}
							<tr class="border-b border-line align-top last:border-b-0">
								<td class="px-3 py-2">
									<div class="font-medium">{field.label}</div>
									<div class="font-mono text-xs text-muted">
										{field.key}{field.search_alias ? ` · ${field.search_alias}:` : ''}
									</div>
								</td>
								<td class="px-3 py-2 whitespace-nowrap">
									{kindLabel(field.kind)}{field.multivalued ? ' ×n' : ''}
								</td>
								<td class="px-3 py-2 text-xs">
									<div class="flex flex-wrap gap-x-3 gap-y-1 whitespace-nowrap">
										<!-- Toggles, not read-outs: these four are the whole edit surface a field has once
									     its key and kind are settled, and each one is a single decision. -->
										<label class="flex items-center gap-1.5">
											<input
												type="checkbox"
												class="rounded border-line text-accent"
												checked={field.required}
												disabled={busy}
												onchange={(event) =>
													amend(field.key, { required: event.currentTarget.checked })}
											/>
											<span>Required</span>
										</label>
										<label class="flex items-center gap-1.5">
											<input
												type="checkbox"
												class="rounded border-line text-accent"
												checked={field.searchable}
												disabled={busy}
												onchange={(event) =>
													amend(field.key, { searchable: event.currentTarget.checked })}
											/>
											<span>Searchable</span>
										</label>
										<label class="flex items-center gap-1.5">
											<input
												type="checkbox"
												class="rounded border-line text-accent"
												checked={field.facetable}
												disabled={busy}
												onchange={(event) =>
													amend(field.key, { facetable: event.currentTarget.checked })}
											/>
											<span>Filterable</span>
										</label>
										<label class="flex items-center gap-1.5">
											<input
												type="checkbox"
												class="rounded border-line text-accent"
												checked={field.read_only}
												disabled={busy}
												onchange={(event) =>
													amend(field.key, { read_only: event.currentTarget.checked })}
											/>
											<span>Read-only</span>
										</label>
									</div>
								</td>
								<td class="tabular px-3 py-2 whitespace-nowrap">
									{field.assets_with_values.toLocaleString()}
								</td>
								<td class="px-3 py-2 whitespace-nowrap">
									<button
										type="button"
										class="rounded border border-line px-1.5 disabled:opacity-40"
										aria-label={`Move ${field.label} up`}
										disabled={busy || index === 0}
										onclick={() => move(index, -1)}>↑</button
									>
									<button
										type="button"
										class="rounded border border-line px-1.5 disabled:opacity-40"
										aria-label={`Move ${field.label} down`}
										disabled={busy || index === fields.length - 1}
										onclick={() => move(index, 1)}>↓</button
									>
								</td>
								<td class="px-3 py-2 text-right whitespace-nowrap">
									<button
										type="button"
										class="text-xs text-state-rights-denied-fg underline"
										disabled={busy}
										onclick={() =>
											(confirmingRemoval = confirmingRemoval === field.key ? null : field.key)}
										aria-expanded={confirmingRemoval === field.key}
									>
										Remove
									</button>
								</td>
							</tr>
							{#if confirmingRemoval === field.key}
								<!--
								The confirmation gets its own full-width row rather than living in the actions cell.
								Driving the real tenant found out why: a sentence carrying a real count ("22 asset(s)
								use it — the values are kept…") widened that one cell enough to reflow every column
								and push the Remove and Cancel buttons off the right edge of the table. A mocked
								fixture with short labels never showed it. A row spanning the table has room for the
								sentence, and the columns above it do not move.
							-->
								<tr class="border-b border-line bg-surface last:border-b-0">
									<td colspan="6" class="px-3 py-2">
										<div class="flex flex-wrap items-center gap-x-3 gap-y-2 text-sm">
											<span>
												Remove <span class="font-mono">{field.key}</span>?
												{#if field.assets_with_values > 0}
													<span class="text-muted">
														{field.assets_with_values.toLocaleString()} asset(s) use it — the values are
														kept and come back if you re-add it as the same kind.
													</span>
												{:else}
													<span class="text-muted">No asset carries a value for it.</span>
												{/if}
											</span>
											<span class="flex items-center gap-2">
												<button
													type="button"
													class="rounded-md bg-state-rights-denied px-2.5 py-1 text-xs font-medium text-state-rights-denied-fg disabled:opacity-50"
													disabled={busy}
													onclick={() => remove(field.key)}
												>
													Remove {field.key}
												</button>
												<button
													type="button"
													class="text-xs underline"
													onclick={() => (confirmingRemoval = null)}
												>
													Cancel
												</button>
											</span>
										</div>
									</td>
								</tr>
							{/if}
						{/each}
					</tbody>
				</table>
			</div>
			<p class="text-xs text-muted">
				A field's kind cannot change once assets carry values for it: the stored values were checked
				against the old kind and nothing re-checks them. Add a new field and move the data instead.
			</p>
		{/if}
	</section>

	<!-- Types come after the fields because a type is a selection *over* them; the page reads in the order
	     the two depend on each other. `onchanged` reloads the field list, since a type edit can change a
	     field's usage picture. -->
	<hr class="border-line" />
	<MetadataTypes {fields} onchanged={load} />

	<!-- Profiles next: a profile's substance is the defaults it applies and the form it chooses, so it can only
	     be read after both of those exist above it. -->
	<hr class="border-line" />
	<UploadProfiles {fields} />

	<!-- Auto-import last, and deliberately after profiles: the two both fill metadata on the way in, and the
	     order they run in is the thing to understand — the file's own answer first, the profile's blanket one
	     only for what is left. Reading them in that order on the page is the cheapest way to say so. -->
	<hr class="border-line" />
	<AutoImport {fields} />
</div>
