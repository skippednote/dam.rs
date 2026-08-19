<!--
	Upload profiles: what an intake already knows about the files arriving through it.

	## Why this lives beside the schema rather than under "uploads"

	A profile's substance is metadata — the defaults it applies and the form it chooses — so it can only be
	understood after the fields and types above it. Putting it with the uploader would be putting the
	*configuration* where the *action* is, and an administrator setting up an intake is doing schema work.

	## The three switches each mean something a person has to weigh

	Defaults apply to everything from a source, and are validated the moment they are typed. Requiring complete
	metadata is a promise the uploader extracts before bytes move, not a server-side refusal — by the time an
	upload finalises its bytes are staged, and refusing then would strand them. And turning tagging off is for
	the deliveries that arrive already described, or must not be machine-read at all.
-->
<script lang="ts">
	import {
		ApiError,
		amendUploadProfile,
		createUploadProfile,
		listMetadataTypes,
		listUploadProfiles,
		removeUploadProfile,
		type MetadataTypeRow,
		type SchemaField,
		type UploadProfile
	} from '$lib/api/client';

	let { fields }: { fields: SchemaField[] } = $props();

	/**
	 * The tenant's metadata types, for the form picker.
	 *
	 * Fetched here rather than threaded through the page, so this component and the types section stay
	 * independent — the list is a handful of rows, and the alternative is the page holding state on behalf of
	 * two children that would each rather own it.
	 */
	let types = $state<MetadataTypeRow[]>([]);

	let profiles = $state<UploadProfile[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let adding = $state(false);
	let editing = $state<string | null>(null);
	let confirmingRemoval = $state<string | null>(null);
	let draft = $state({ key: '', label: '' });
	/** The default being edited, as `field` → text. Kept flat because that is what the form edits. */
	let defaultDraft = $state<Record<string, string>>({});

	async function load() {
		error = '';
		try {
			profiles = await listUploadProfiles();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load upload profiles.';
		}
	}

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
			const created = await createUploadProfile({
				key: draft.key.trim(),
				label: draft.label.trim() || draft.key.trim()
			});
			adding = false;
			// Opened straight into editing: a profile with no defaults and no form is a name, and the next
			// thing to do is always to say what it means.
			editing = created.id;
			defaultDraft = {};
			draft = { key: '', label: '' };
			return `Added ${created.key}.`;
		});
	}

	function toggle(profile: UploadProfile, change: Record<string, unknown>, said: string) {
		void run(async () => {
			await amendUploadProfile(profile.id, change);
			return `${profile.label}: ${said}`;
		});
	}

	function saveDefaults(profile: UploadProfile) {
		// Empty values are dropped rather than sent as empty strings: "" is a value the validator would accept
		// for a text field, so sending it would silently default every asset to blank.
		const defaults: Record<string, string> = {};
		for (const [key, value] of Object.entries(defaultDraft)) {
			if (value.trim()) defaults[key] = value.trim();
		}
		void run(async () => {
			await amendUploadProfile(profile.id, { defaults });
			const count = Object.keys(defaults).length;
			return `${profile.label}: ${count === 0 ? 'defaults cleared' : `${count} default(s) saved`}.`;
		});
	}

	function startEditing(profile: UploadProfile) {
		editing = profile.id;
		const existing = (profile.defaults ?? {}) as Record<string, unknown>;
		defaultDraft = Object.fromEntries(
			Object.entries(existing).map(([key, value]) => [key, String(value ?? '')])
		);
	}

	function remove(profile: UploadProfile) {
		void run(async () => {
			await removeUploadProfile(profile.id);
			confirmingRemoval = null;
			return `Removed ${profile.key}. Assets that arrived under it keep everything they hold.`;
		});
	}

	/** Only fields a profile may write: a read-only field is maintained by the system. */
	const settable = $derived(fields.filter((field) => !field.read_only));

	$effect(() => {
		void load();
		void (async () => {
			try {
				types = await listMetadataTypes();
			} catch {
				// The picker degrades to "chosen by file type", which is the behaviour when no profile names a
				// form. An empty select is better than a section that refuses to render.
				types = [];
			}
		})();
	});
</script>

<section class="space-y-3" aria-label="Upload profiles">
	<div class="flex flex-wrap items-end justify-between gap-3">
		<div>
			<h2 class="text-lg font-semibold tracking-tight">Upload profiles</h2>
			<p class="mt-1 max-w-2xl text-sm text-muted">
				One per kind of intake — a photographer's drop, a partner's delivery. A profile says what is
				already true of everything arriving that way, which form those assets get, and whether they
				may be tagged automatically.
			</p>
		</div>
		<button
			type="button"
			class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised"
			onclick={() => (adding = !adding)}
			aria-expanded={adding}
		>
			Add a profile
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
					placeholder="press"
					required
				/>
			</label>
			<label class="flex flex-col gap-1 text-sm">
				<span class="text-xs font-medium text-muted">Label</span>
				<input
					class="w-48 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={draft.label}
					placeholder="Press delivery"
				/>
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
				The key is what an uploader names in its request, so it travels in integrations — pick
				something short and stable.
			</p>
		</form>
	{/if}

	{#if profiles.length === 0}
		<p class="text-sm text-muted">
			No profiles yet. Uploads arrive with no defaults, the form chosen by the file's type, and
			automatic tagging on.
		</p>
	{:else}
		<ul class="space-y-2">
			{#each profiles as profile (profile.id)}
				<li class="rounded-md border border-line">
					<div class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2">
						<span class="font-medium">{profile.label}</span>
						<span class="font-mono text-xs text-muted">{profile.key}</span>
						{#if profile.is_default}
							<span
								class="rounded bg-state-rights-allowed/18 px-1.5 py-0.5 text-xs font-medium text-state-rights-allowed-fg"
							>
								default
							</span>
						{/if}
						<span class="text-xs text-muted">
							{Object.keys(profile.defaults ?? {}).length} default(s)
							{#if profile.require_complete}· requires metadata{/if}
							{#if !profile.ai_tags_enabled}· no tagging{/if}
						</span>
						<span class="ml-auto flex items-center gap-3 text-xs">
							<button
								type="button"
								class="underline"
								onclick={() => (editing === profile.id ? (editing = null) : startEditing(profile))}
								aria-expanded={editing === profile.id}
							>
								{editing === profile.id ? 'Done' : 'Edit'}
							</button>
							{#if !profile.is_default}
								<button
									type="button"
									class="underline"
									disabled={busy}
									onclick={() => toggle(profile, { is_default: true }, 'is now the default.')}
								>
									Make default
								</button>
							{/if}
							<button
								type="button"
								class="text-state-rights-denied-fg underline"
								disabled={busy}
								onclick={() =>
									(confirmingRemoval = confirmingRemoval === profile.id ? null : profile.id)}
								aria-expanded={confirmingRemoval === profile.id}
							>
								Remove
							</button>
						</span>
					</div>

					{#if confirmingRemoval === profile.id}
						<div
							class="flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-line bg-surface px-3 py-2 text-sm"
						>
							<span>
								Remove <span class="font-mono">{profile.key}</span>?
								<span class="text-muted">
									Assets that already arrived under it keep every value they hold; only future
									uploads change.
								</span>
							</span>
							<button
								type="button"
								class="rounded-md bg-state-rights-denied px-2.5 py-1 text-xs font-medium text-state-rights-denied-fg disabled:opacity-50"
								disabled={busy}
								onclick={() => remove(profile)}
							>
								Remove {profile.key}
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

					{#if editing === profile.id}
						<div class="space-y-3 border-t border-line bg-surface px-3 py-3">
							<label class="flex items-center justify-between gap-2 text-sm">
								<span>Form for these uploads</span>
								<select
									class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
									value={profile.metadata_type_id ?? ''}
									disabled={busy}
									onchange={(event) =>
										toggle(
											profile,
											{ metadata_type_id: event.currentTarget.value || null },
											'form changed.'
										)}
								>
									<!-- Null is not "no form": it means the file's own type decides, which is what
									     happens when no profile names one. -->
									<option value="">Chosen by file type</option>
									{#each types as type (type.id)}
										<option value={type.id}>{type.label}</option>
									{/each}
								</select>
							</label>

							<label class="flex items-start gap-2 text-sm">
								<input
									type="checkbox"
									class="mt-0.5 rounded border-line text-accent"
									checked={profile.require_complete}
									disabled={busy}
									onchange={(event) =>
										toggle(
											profile,
											{ require_complete: event.currentTarget.checked },
											event.currentTarget.checked
												? 'the uploader will insist on required fields.'
												: 'the uploader will no longer insist.'
										)}
								/>
								<span>
									Insist on required metadata
									<span class="block text-xs text-muted">
										Applied by the uploader before bytes move. The server will not refuse a finished
										upload over it — by then the bytes are stored, and refusing would lose them.
									</span>
								</span>
							</label>

							<label class="flex items-start gap-2 text-sm">
								<input
									type="checkbox"
									class="mt-0.5 rounded border-line text-accent"
									checked={profile.ai_tags_enabled}
									disabled={busy}
									onchange={(event) =>
										toggle(
											profile,
											{ ai_tags_enabled: event.currentTarget.checked },
											event.currentTarget.checked
												? 'automatic tagging is on.'
												: 'automatic tagging is off.'
										)}
								/>
								<span>
									Tag automatically
									<span class="block text-xs text-muted">
										Off for deliveries that arrive already described, or that must not be
										machine-read.
									</span>
								</span>
							</label>

							{#if settable.length > 0}
								<div class="space-y-1">
									<p class="text-xs font-medium text-muted">
										Applied to everything from this intake
									</p>
									{#each settable as field (field.key)}
										<label class="flex items-center justify-between gap-2 text-sm">
											<span>{field.label}</span>
											<input
												class="w-56 rounded-md border border-line bg-bg px-2 py-1 text-sm"
												value={defaultDraft[field.key] ?? ''}
												placeholder="none"
												oninput={(event) => (defaultDraft[field.key] = event.currentTarget.value)}
											/>
										</label>
									{/each}
									<button
										type="button"
										class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
										disabled={busy}
										onclick={() => saveDefaults(profile)}
									>
										Save defaults
									</button>
									<p class="text-xs text-muted">
										A default fills a field the upload left empty; it never overwrites what somebody
										typed.
									</p>
								</div>
							{/if}
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>
