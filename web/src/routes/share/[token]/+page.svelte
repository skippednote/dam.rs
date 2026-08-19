<script lang="ts">
	/**
	 * The portal: what a share-link recipient sees.
	 *
	 * No session, no nav, no key — the token in the URL is the credential, and the person here has no account.
	 * The page shows exactly what the API grants: the asset's name always (the share's creator chose to
	 * disclose it), the pixels only when rights permit distribution, and a download that spends the limit.
	 *
	 * Refusals are rendered in the recipient's terms, because the recipient is who reads them: "expired" means
	 * ask for a new link, "passcode required" means look in the email, "not licensed" means the sender's
	 * problem, not yours.
	 */
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { ApiError, portalDownload, portalView, type PortalView } from '$lib/api/client';
	import { deliveryUrl } from '$lib/api/client';

	const token = page.params.token ?? '';

	let view = $state<PortalView | null>(null);
	let needsPasscode = $state(false);
	let passcode = $state('');
	let error = $state('');
	let downloading = $state(false);
	let remaining = $state<number | null>(null);

	async function load() {
		error = '';
		try {
			view = await portalView(token, passcode || undefined);
			needsPasscode = false;
			remaining = view.downloads_remaining ?? null;
		} catch (caught) {
			if (caught instanceof ApiError && caught.status === 401) {
				// Required and wrong are different messages from the server, shown as-is: one says look for
				// the passcode, the other says re-read it.
				needsPasscode = true;
				error = passcode ? caught.message : '';
			} else {
				error = caught instanceof ApiError ? caught.message : 'This link could not be opened.';
			}
		}
	}

	async function download() {
		downloading = true;
		error = '';
		try {
			const grant = await portalDownload(token, passcode || undefined);
			remaining = grant.downloads_remaining ?? null;
			// Navigated, not window.open: a popup blocker eating the download reads as the link being broken.
			location.href = deliveryUrl(grant.url);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'The download could not be started.';
		} finally {
			downloading = false;
		}
	}

	function bytes(n: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}

	onMount(load);
</script>

<svelte:head>
	<title>Shared file</title>
</svelte:head>

<div class="mx-auto flex min-h-[80vh] max-w-2xl flex-col justify-center gap-6 p-8">
	{#if view}
		<div>
			<h1 class="text-xl font-semibold tracking-tight break-all">{view.filename}</h1>
			<p class="mt-1 text-sm text-muted">
				<span class="font-mono">{view.mime}</span>
				· <span class="tabular">{bytes(view.bytes)}</span>
				{#if view.width && view.height}
					· <span class="tabular">{view.width} × {view.height}</span>
				{/if}
			</p>
		</div>

		{#if view.preview_url}
			<img
				src={deliveryUrl(view.preview_url)}
				alt={view.filename}
				class="image-well max-h-[50vh] w-full rounded-lg object-contain"
			/>
		{:else}
			<div class="image-well flex h-48 items-center justify-center rounded-lg">
				<p class="max-w-sm px-6 text-center text-sm text-muted">
					{view.preview_unavailable ?? 'No preview is available.'}
				</p>
			</div>
		{/if}

		<div class="flex flex-wrap items-center gap-3">
			{#if view.download_allowed}
				<button
					type="button"
					class="rounded-md bg-accent px-4 py-2 text-sm font-medium text-accent-fg disabled:opacity-50"
					onclick={download}
					disabled={downloading || remaining === 0}
				>
					{downloading ? 'Preparing…' : 'Download'}
				</button>
			{:else}
				<!-- The server already said no (rights, usually) — a button that would 403 is a broken button. -->
				<span class="text-sm text-muted">Downloading isn't available for this file.</span>
			{/if}
			{#if remaining !== null}
				<span class="tabular text-xs text-muted" role="status">
					{remaining} download{remaining === 1 ? '' : 's'} remaining
				</span>
			{/if}
			{#if view.expires_at}
				<span class="text-xs text-muted">
					Link expires <time datetime={view.expires_at}
						>{new Date(view.expires_at).toLocaleDateString()}</time
					>
				</span>
			{/if}
		</div>
		{#if error}
			<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
		{/if}
	{:else if needsPasscode}
		<form
			class="space-y-3"
			onsubmit={(event) => {
				event.preventDefault();
				void load();
			}}
		>
			<h1 class="text-xl font-semibold tracking-tight">This link needs a passcode</h1>
			<p class="text-sm text-muted">It was sent alongside the link — check the same message.</p>
			<div class="flex gap-2">
				<label class="sr-only" for="passcode">Passcode</label>
				<input
					id="passcode"
					type="password"
					class="rounded-md border border-line bg-bg px-3 py-2 text-sm"
					bind:value={passcode}
					autocomplete="off"
				/>
				<button
					type="submit"
					class="rounded-md bg-accent px-4 py-2 text-sm font-medium text-accent-fg"
				>
					Open
				</button>
			</div>
			{#if error}
				<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
			{/if}
		</form>
	{:else if error}
		<div>
			<h1 class="text-xl font-semibold tracking-tight">This link does not work any more</h1>
			<p role="alert" class="mt-2 text-sm text-muted">{error}</p>
			<p class="mt-4 text-sm text-muted">
				If you were expecting a file, ask the person who sent it for a fresh link.
			</p>
		</div>
	{:else}
		<p class="text-sm text-muted" role="status">Opening…</p>
	{/if}
</div>
