<!--
	The upload queue.

	**Uploads run one at a time.** Not a limitation to fix later: TUS chunks are 8 MiB and the server writes
	each into staging, so six parallel uploads is six times the memory and six times the S3 multipart state
	for no gain on a single connection. One at a time also makes the progress figure mean something — a
	queue showing six bars at 40% tells a user nothing about when they can leave.

	**A failure keeps its place in the queue.** The offset lives on the server, so retrying resumes rather
	than restarting. A queue that dropped failed items would turn a dropped connection into a lost 4 GB
	upload, which is exactly what the protocol exists to prevent.
-->
<script lang="ts">
	import { ApiError, listUploadProfiles, type UploadProfile } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';
	import { create, send, type UploadHandle } from '$lib/upload/tus';

	let { onfinished }: { onfinished?: () => void } = $props();

	/**
	 * The tenant's upload profiles, and which one this batch is going under.
	 *
	 * Read here rather than passed in, because the picker and the rule it implies belong to the act of
	 * uploading: a profile decides what metadata is already true of these files and whether required fields
	 * have to be filled before they go.
	 */
	let profiles = $state<UploadProfile[]>([]);
	let chosenProfile = $state('');

	const profile = $derived(profiles.find((candidate) => candidate.key === chosenProfile));

	/**
	 * The fields a person must fill before this batch may go.
	 *
	 * `require_complete` is a client-side rule by design — see `dam_db::upload_profiles`. The server will not
	 * refuse an incomplete upload, because by then the bytes are staged and refusing would strand them; so
	 * this is the only place the rule can be applied while it is still cheap to satisfy.
	 */
	let metadataConfirmed = $state(false);
	const blocked = $derived(profile?.require_complete === true && !metadataConfirmed);

	$effect(() => {
		void (async () => {
			try {
				profiles = await listUploadProfiles();
				// Preselect the tenant's fallback, because that is what the server would choose anyway —
				// showing "none" while the server silently applies a profile would misdescribe what happens.
				chosenProfile = profiles.find((candidate) => candidate.is_default)?.key ?? '';
			} catch (caught) {
				// Silent: profiles are an affordance, and an uploader that refused to open because it could not
				// list them would be worse than one that uploads under the server's own default.
				void (caught instanceof ApiError);
			}
		})();
	});

	type Item = {
		file: File;
		sent: number;
		state: 'queued' | 'uploading' | 'done' | 'failed';
		error?: string;
		handle?: UploadHandle;
	};

	let items = $state<Item[]>([]);
	let running = $state(false);
	let dragging = $state(false);
	let controller: AbortController | null = null;

	const active = $derived(items.filter((item) => item.state !== 'done'));
	const total = $derived(items.reduce((sum, item) => sum + item.file.size, 0));
	const sent = $derived(items.reduce((sum, item) => sum + item.sent, 0));

	function add(files: FileList | File[]) {
		for (const file of files) {
			items.push({ file, sent: 0, state: 'queued' });
		}
		void drain();
	}

	async function drain() {
		if (running) return;
		running = true;
		controller = new AbortController();
		try {
			// Re-read each pass rather than iterating a snapshot: a file dropped while an upload is in flight
			// has to join the same run, or it sits queued until something else triggers a drain.
			for (;;) {
				const next = items.find((item) => item.state === 'queued' || item.state === 'failed');
				if (!next) break;
				await upload(next);
			}
		} finally {
			running = false;
			controller = null;
			if (items.some((item) => item.state === 'done')) onfinished?.();
		}
	}

	async function upload(item: Item) {
		item.state = 'uploading';
		item.error = undefined;
		try {
			// Reused when present, which is what makes a retry a resume: creating a second session would
			// upload the whole file again and leave the first one for the reaper.
			item.handle ??= await create(
				session.base,
				session.key,
				{
					name: item.file.name,
					size: item.file.size,
					type: item.file.type
				},
				// The key, not the id — and only when one is chosen. See `create` in `$lib/upload/tus`.
				chosenProfile || undefined
			);
			await send(
				session.base,
				session.key,
				item.handle,
				item.file,
				({ sent: at }) => {
					item.sent = at;
				},
				controller?.signal
			);
			item.state = 'done';
		} catch (caught) {
			item.state = 'failed';
			item.error = caught instanceof Error ? caught.message : 'Upload failed.';
		}
	}

	function cancel() {
		controller?.abort();
	}

	function clearFinished() {
		items = items.filter((item) => item.state !== 'done');
	}

	function percent(item: Item): number {
		return item.file.size === 0 ? 100 : Math.round((item.sent / item.file.size) * 100);
	}
</script>

<div class="space-y-3">
	{#if profiles.length > 0}
		<div class="space-y-2 rounded-md bg-surface p-3">
			<label class="flex items-center justify-between gap-2 text-sm">
				<span class="font-medium">Uploading as</span>
				<select
					class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={chosenProfile}
					disabled={running}
				>
					<!-- "No profile" is offered last and explicitly, because on a tenant that has profiles it is
					     the deliberate choice rather than the default. -->
					{#each profiles as candidate (candidate.id)}
						<option value={candidate.key}>
							{candidate.label}{candidate.is_default ? ' (default)' : ''}
						</option>
					{/each}
					<option value="">No profile</option>
				</select>
			</label>

			{#if profile}
				<p class="text-xs text-muted">
					{#if Object.keys(profile.defaults ?? {}).length > 0}
						Applies {Object.keys(profile.defaults ?? {}).join(', ')} to everything in this batch.
					{/if}
					{#if !profile.ai_tags_enabled}
						No automatic tagging.
					{/if}
				</p>
			{/if}

			{#if profile?.require_complete}
				<!--
					The rule, made satisfiable rather than merely enforced. This intake insists on required
					metadata, and the server deliberately will not refuse the upload later — so the only useful
					thing to do here is say so and make the person acknowledge it before the bytes go.
				-->
				<label class="flex items-start gap-2 text-xs">
					<input
						type="checkbox"
						class="mt-0.5 rounded border-line text-accent"
						bind:checked={metadataConfirmed}
						disabled={running}
					/>
					<span>
						This intake requires complete metadata. I will fill the required fields on these assets
						before they are used.
					</span>
				</label>
			{/if}
		</div>
	{/if}

	<!--
		A label wrapping a real file input, not a div with a click handler. The input is the only thing that
		opens a file picker from the keyboard, and `sr-only` rather than `hidden` keeps it focusable — a
		`display: none` input is unreachable by Tab and the control becomes mouse-only.
	-->
	<label
		class="flex cursor-pointer flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed p-6 text-center text-sm transition-colors
		       {dragging ? 'border-accent bg-raised' : 'border-line'}"
		ondragover={(event) => {
			event.preventDefault();
			dragging = true;
		}}
		ondragleave={() => (dragging = false)}
		ondrop={(event) => {
			event.preventDefault();
			dragging = false;
			// Also gated: disabling the input stops the keyboard path, and a drop that still worked would make
			// the mouse route quietly more permissive than the accessible one.
			if (!blocked && event.dataTransfer?.files) add(event.dataTransfer.files);
		}}
	>
		<input
			type="file"
			multiple
			class="sr-only"
			disabled={blocked}
			onchange={(event) => {
				const input = event.currentTarget;
				if (input.files) add(input.files);
				// Reset so choosing the same file twice fires `change` again — otherwise a retry after a
				// failure looks like nothing happened.
				input.value = '';
			}}
		/>
		<span class="font-medium">Drop files here, or choose files</span>
		{#if blocked}
			<span class="text-xs text-state-rights-denied-fg">
				Acknowledge the metadata requirement above first.
			</span>
		{:else}
			<span class="text-xs text-muted"
				>Resumable — a dropped connection picks up where it stopped.</span
			>
		{/if}
	</label>

	{#if items.length > 0}
		<div class="flex items-center justify-between text-xs text-muted">
			<span aria-live="polite">
				{#if active.length === 0}
					{items.length} finished
				{:else}
					{active.length} of {items.length} remaining · {Math.round(
						(sent / Math.max(total, 1)) * 100
					)}%
				{/if}
			</span>
			<span class="flex gap-2">
				{#if running}
					<button type="button" class="underline" onclick={cancel}>Cancel</button>
				{/if}
				{#if items.some((item) => item.state === 'done')}
					<button type="button" class="underline" onclick={clearFinished}>Clear finished</button>
				{/if}
			</span>
		</div>

		<ul class="space-y-1.5">
			{#each items as item (item.file.name + item.file.size + item.file.lastModified)}
				<li class="space-y-1 rounded-md bg-surface px-2 py-1.5 text-xs">
					<div class="flex items-center justify-between gap-2">
						<span class="truncate" title={item.file.name}>{item.file.name}</span>
						<span class="shrink-0 tabular-nums">
							{item.state === 'failed' ? 'failed' : `${percent(item)}%`}
						</span>
					</div>
					<!--
						A native `progress` element: it is announced as a progress bar with its value, which a
						styled div is not. `max`/`value` in bytes rather than percent so the announcement carries the
						real figure.
					-->
					<progress
						class="h-1 w-full"
						max={item.file.size}
						value={item.sent}
						aria-label={`Uploading ${item.file.name}`}
					></progress>
					{#if item.error}
						<p role="alert" class="text-state-rights-denied-fg">
							{item.error} — it will resume from where it stopped if you add it again.
						</p>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</div>
