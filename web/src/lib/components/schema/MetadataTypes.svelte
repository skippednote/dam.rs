<!--
	Metadata types: which of the tenant's fields apply to which kind of asset.

	## Why this sits under the field list rather than beside it

	A type is a *selection* over the vocabulary above it, so it cannot be understood before the fields are.
	Reading the page top to bottom is "here are the fields this tenant has" then "here is which of them each
	kind of asset shows", which is the order the two things depend on each other in.

	## Each type carries its asset count, for the same reason a field carries its usage count

	Removing a type re-forms every asset that referenced it. Those assets are not broken — they fall back to
	the default — but their form changes, and an administrator deciding whether to remove a type should not
	have to go and ask how many that is.
-->
<script lang="ts">
	import {
		ApiError,
		amendMetadataType,
		defineMetadataType,
		listMetadataTypes,
		removeMetadataType,
		type MetadataTypeRow,
		type SchemaField
	} from '$lib/api/client';

	let { fields, onchanged }: { fields: SchemaField[]; onchanged: () => void } = $props();

	/** The media classes ingest sorts a file into. Coarser than mime on purpose — see `media_class`. */
	const CLASSES = ['image', 'video', 'audio', 'document', 'archive'] as const;

	let types = $state<MetadataTypeRow[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let adding = $state(false);
	let editing = $state<string | null>(null);
	let confirmingRemoval = $state<string | null>(null);
	let draft = $state({ key: '', label: '', applies_to: [] as string[] });

	async function load() {
		error = '';
		try {
			types = await listMetadataTypes();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load metadata types.';
		}
	}

	async function run(work: () => Promise<string>) {
		busy = true;
		error = '';
		try {
			notice = await work();
			await load();
			onchanged();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'That change could not be made.';
		} finally {
			busy = false;
		}
	}

	function add(event: SubmitEvent) {
		event.preventDefault();
		void run(async () => {
			const created = await defineMetadataType({
				key: draft.key.trim(),
				label: draft.label.trim() || draft.key.trim(),
				applies_to: draft.applies_to,
				field_keys: []
			});
			adding = false;
			// Opened straight into editing: a type with no fields shows an empty form, so the next thing to do
			// is always to choose some.
			editing = created.id;
			draft = { key: '', label: '', applies_to: [] };
			return `Added ${created.key}. Choose the fields it should show.`;
		});
	}

	/** Adds or removes one field, sending the whole list — which is the endpoint's contract. */
	function toggleField(type: MetadataTypeRow, key: string, include: boolean) {
		const next = include
			? [...type.field_keys, key]
			: type.field_keys.filter((existing) => existing !== key);
		void run(async () => {
			await amendMetadataType(type.id, { field_keys: next });
			return `${type.label}: ${include ? 'added' : 'removed'} ${key}.`;
		});
	}

	function move(type: MetadataTypeRow, index: number, delta: number) {
		const next = index + delta;
		if (next < 0 || next >= type.field_keys.length) return;
		const keys = [...type.field_keys];
		[keys[index], keys[next]] = [keys[next], keys[index]];
		void run(async () => {
			await amendMetadataType(type.id, { field_keys: keys });
			return `${type.label}: field order saved.`;
		});
	}

	function toggleClass(type: MetadataTypeRow, media: string, include: boolean) {
		const next = include
			? [...type.applies_to, media]
			: type.applies_to.filter((existing) => existing !== media);
		void run(async () => {
			await amendMetadataType(type.id, { applies_to: next });
			return `${type.label}: ${include ? 'now' : 'no longer'} the form for ${media} files.`;
		});
	}

	function makeDefault(type: MetadataTypeRow) {
		void run(async () => {
			await amendMetadataType(type.id, { is_default: true });
			return `${type.label} is now the fallback for anything that matches no other type.`;
		});
	}

	function remove(type: MetadataTypeRow) {
		void run(async () => {
			await removeMetadataType(type.id);
			confirmingRemoval = null;
			const affected =
				type.assets > 0
					? ` The ${type.assets.toLocaleString()} asset(s) that used it fall back to the default form; no metadata was deleted.`
					: '';
			return `Removed ${type.key}.${affected}`;
		});
	}

	$effect(() => {
		void load();
	});
</script>

<!-- Named, so it is a landmark: a screen-reader user can jump between "the fields" and "which forms use
     them" instead of walking the whole page, and it gives tests an unambiguous scope for controls whose
     labels repeat across both sections. -->
<section class="space-y-3" aria-label="Asset types">
	<div class="flex flex-wrap items-end justify-between gap-3">
		<div>
			<h2 class="text-lg font-semibold tracking-tight">Asset types</h2>
			<p class="mt-1 max-w-2xl text-sm text-muted">
				A type decides which of the fields above an asset shows — so a video need not carry
				print-resolution fields, and an archive need not carry alt text. Uploads are sorted into a
				type by what kind of file they are.
			</p>
		</div>
		<button
			type="button"
			class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised"
			onclick={() => (adding = !adding)}
			aria-expanded={adding}
		>
			Add a type
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
					class="w-36 rounded-md border border-line bg-bg px-2 py-1 font-mono text-sm"
					bind:value={draft.key}
					placeholder="video"
					required
				/>
			</label>
			<label class="flex flex-col gap-1 text-sm">
				<span class="text-xs font-medium text-muted">Label</span>
				<input
					class="w-44 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={draft.label}
					placeholder="Video"
				/>
			</label>
			<fieldset class="flex flex-col gap-1">
				<legend class="text-xs font-medium text-muted">Used for uploads of</legend>
				<div class="flex flex-wrap gap-x-3 gap-y-1 text-sm">
					{#each CLASSES as media (media)}
						<label class="flex items-center gap-1.5">
							<input
								type="checkbox"
								class="rounded border-line text-accent"
								checked={draft.applies_to.includes(media)}
								onchange={(event) =>
									(draft.applies_to = event.currentTarget.checked
										? [...draft.applies_to, media]
										: draft.applies_to.filter((existing) => existing !== media))}
							/>
							<span>{media}</span>
						</label>
					{/each}
				</div>
			</fieldset>
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
		</form>
	{/if}

	{#if types.length === 0}
		<p class="text-sm text-muted">
			No types yet — every asset shows every field. Adding one narrows the form for the kind of file
			it covers; anything not covered keeps showing everything.
		</p>
	{:else}
		<ul class="space-y-2">
			{#each types as type (type.id)}
				<li class="rounded-md border border-line">
					<div class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2">
						<span class="font-medium">{type.label}</span>
						<span class="font-mono text-xs text-muted">{type.key}</span>
						{#if type.is_default}
							<span
								class="rounded bg-state-rights-allowed/18 px-1.5 py-0.5 text-xs font-medium text-state-rights-allowed-fg"
							>
								fallback
							</span>
						{/if}
						<span class="tabular text-xs text-muted">
							{type.field_keys.length} field{type.field_keys.length === 1 ? '' : 's'} ·
							{type.assets.toLocaleString()} asset{type.assets === 1 ? '' : 's'}
						</span>
						<span class="ml-auto flex items-center gap-3 text-xs">
							<button
								type="button"
								class="underline"
								onclick={() => (editing = editing === type.id ? null : type.id)}
								aria-expanded={editing === type.id}
							>
								{editing === type.id ? 'Done' : 'Edit fields'}
							</button>
							{#if !type.is_default}
								<button
									type="button"
									class="underline"
									disabled={busy}
									onclick={() => makeDefault(type)}
								>
									Make fallback
								</button>
							{/if}
							<button
								type="button"
								class="text-state-rights-denied-fg underline"
								disabled={busy}
								onclick={() => (confirmingRemoval = confirmingRemoval === type.id ? null : type.id)}
								aria-expanded={confirmingRemoval === type.id}
							>
								Remove
							</button>
						</span>
					</div>

					{#if confirmingRemoval === type.id}
						<div
							class="flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-line bg-surface px-3 py-2 text-sm"
						>
							<span>
								Remove <span class="font-mono">{type.key}</span>?
								{#if type.assets > 0}
									<span class="text-muted">
										{type.assets.toLocaleString()} asset(s) use it. They keep every value they hold and
										fall back to the default form — nothing is deleted.
									</span>
								{/if}
							</span>
							<button
								type="button"
								class="rounded-md bg-state-rights-denied px-2.5 py-1 text-xs font-medium text-state-rights-denied-fg disabled:opacity-50"
								disabled={busy}
								onclick={() => remove(type)}
							>
								Remove {type.key}
							</button>
							<button
								type="button"
								class="text-xs underline"
								onclick={() => (confirmingRemoval = null)}
							>
								Cancel
							</button>
						</div>
					{/if}

					{#if editing === type.id}
						<div class="space-y-3 border-t border-line bg-surface px-3 py-3">
							<fieldset class="space-y-1">
								<legend class="text-xs font-medium text-muted">Used for uploads of</legend>
								<div class="flex flex-wrap gap-x-3 gap-y-1 text-sm">
									{#each CLASSES as media (media)}
										<label class="flex items-center gap-1.5">
											<input
												type="checkbox"
												class="rounded border-line text-accent"
												checked={type.applies_to.includes(media)}
												disabled={busy}
												onchange={(event) => toggleClass(type, media, event.currentTarget.checked)}
											/>
											<span>{media}</span>
										</label>
									{/each}
								</div>
							</fieldset>

							<div class="space-y-1">
								<p class="text-xs font-medium text-muted">
									Fields on this form, in the order they appear
								</p>
								{#if type.field_keys.length === 0}
									<p class="text-sm text-muted">
										None yet. An asset of this type shows no editable metadata at all until you add
										some.
									</p>
								{:else}
									<ol class="space-y-1">
										{#each type.field_keys as key, index (key)}
											<li class="flex items-center gap-2 text-sm">
												<span class="tabular w-6 text-xs text-muted">{index + 1}.</span>
												<span class="flex-1">
													{fields.find((field) => field.key === key)?.label ?? key}
													<span class="font-mono text-xs text-muted">{key}</span>
												</span>
												<button
													type="button"
													class="rounded border border-line px-1.5 text-xs disabled:opacity-40"
													aria-label={`Move ${key} up in ${type.label}`}
													disabled={busy || index === 0}
													onclick={() => move(type, index, -1)}>↑</button
												>
												<button
													type="button"
													class="rounded border border-line px-1.5 text-xs disabled:opacity-40"
													aria-label={`Move ${key} down in ${type.label}`}
													disabled={busy || index === type.field_keys.length - 1}
													onclick={() => move(type, index, 1)}>↓</button
												>
												<button
													type="button"
													class="text-xs underline"
													disabled={busy}
													onclick={() => toggleField(type, key, false)}
												>
													Remove
												</button>
											</li>
										{/each}
									</ol>
								{/if}
							</div>

							{#if fields.some((field) => !type.field_keys.includes(field.key))}
								<div class="space-y-1">
									<p class="text-xs font-medium text-muted">Fields not on this form</p>
									<div class="flex flex-wrap gap-2">
										{#each fields.filter((field) => !type.field_keys.includes(field.key)) as field (field.key)}
											<button
												type="button"
												class="rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised disabled:opacity-50"
												disabled={busy}
												onclick={() => toggleField(type, field.key, true)}
											>
												+ {field.label}
											</button>
										{/each}
									</div>
								</div>
							{/if}
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>
