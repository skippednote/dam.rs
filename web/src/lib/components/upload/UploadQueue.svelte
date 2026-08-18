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
	import { session } from '$lib/api/session.svelte';
	import { create, send, type UploadHandle } from '$lib/upload/tus';

	let { onfinished }: { onfinished?: () => void } = $props();

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
			item.handle ??= await create(session.base, session.key, {
				name: item.file.name,
				size: item.file.size,
				type: item.file.type
			});
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
	<!--
		A label wrapping a real file input, not a div with a click handler. The input is the only thing that
		opens a file picker from the keyboard, and `sr-only` rather than `hidden` keeps it focusable — a
		`display: none` input is unreachable by Tab and the control becomes mouse-only.
	-->
	<label
		class="flex cursor-pointer flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed p-6 text-center text-sm transition-colors
		       {dragging ? 'border-accent bg-surface' : 'border-state-neutral'}"
		ondragover={(event) => {
			event.preventDefault();
			dragging = true;
		}}
		ondragleave={() => (dragging = false)}
		ondrop={(event) => {
			event.preventDefault();
			dragging = false;
			if (event.dataTransfer?.files) add(event.dataTransfer.files);
		}}
	>
		<input
			type="file"
			multiple
			class="sr-only"
			onchange={(event) => {
				const input = event.currentTarget;
				if (input.files) add(input.files);
				// Reset so choosing the same file twice fires `change` again — otherwise a retry after a
				// failure looks like nothing happened.
				input.value = '';
			}}
		/>
		<span class="font-medium">Drop files here, or choose files</span>
		<span class="text-xs text-muted"
			>Resumable — a dropped connection picks up where it stopped.</span
		>
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
