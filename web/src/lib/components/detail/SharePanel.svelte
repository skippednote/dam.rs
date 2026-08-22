<!--
	Sharing one asset, from the detail panel.

	The token is shown **once**, exactly like an issued API key and for the same reason: the server stores a
	digest, so a lost link cannot be recovered — only revoked and re-created. The panel says so next to the
	copy button rather than in documentation nobody reads at the moment it matters.

	The options are the three that change what the link *is* — expiry, a download limit, a passcode — plus
	whether the recipient gets the original. Everything else (the recipient's experience, rights) is decided at
	delivery, where it is enforced, not here where it would merely be recorded.
-->
<script lang="ts">
	import { ApiError, createShare, type CreatedShare } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let { assetId }: { assetId: string } = $props();

	let open = $state(false);
	let expires = $state<'never' | '24' | '168' | '720'>('168');
	let maxDownloads = $state('');
	let passcode = $state('');
	let allowOriginal = $state(false);
	let created = $state<CreatedShare | null>(null);
	let error = $state('');
	let copied = $state(false);

	// A new asset resets the flow: a token displayed against the wrong filename is a mis-sent link.
	$effect(() => {
		void assetId;
		open = false;
		created = null;
		error = '';
		copied = false;
	});

	/**
	 * The URL a recipient opens.
	 *
	 * The *web app's* origin plus the portal route — not the API's. The portal page is ours; it calls the API
	 * itself. Composed at display time so the same share works whatever host the app is served from.
	 */
	const portalUrl = $derived(
		created
			? `${typeof location === 'undefined' ? '' : location.origin}/share/${created.token}`
			: ''
	);

	async function make(event: SubmitEvent) {
		event.preventDefault();
		error = '';
		try {
			created = await createShare({
				asset_id: assetId,
				expires_in_hours: expires === 'never' ? undefined : Number(expires),
				max_downloads: maxDownloads ? Number(maxDownloads) : undefined,
				passcode: passcode || undefined,
				allow_original: allowOriginal
			});
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not create the share.';
		}
	}

	async function copy() {
		await navigator.clipboard.writeText(portalUrl);
		copied = true;
	}
</script>

<div>
	<h3 class="mb-2 text-xs font-semibold tracking-wide text-muted uppercase">Share</h3>

	{#if created}
		<div class="space-y-2 rounded-md bg-surface p-3">
			<p class="text-xs text-muted">
				This link is shown once — the server keeps only a digest, so if it is lost, revoke it on the
				Shares page and make a new one.
			</p>
			<div class="flex items-center gap-2">
				<input
					readonly
					class="min-w-0 flex-1 rounded-md border border-line bg-bg px-2 py-1 font-mono text-xs"
					value={portalUrl}
					aria-label="Share link"
					onfocus={(event) => event.currentTarget.select()}
				/>
				<button
					type="button"
					class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg"
					onclick={copy}
				>
					{copied ? 'Copied' : 'Copy'}
				</button>
			</div>
			{#if session.base.startsWith('http://localhost') || session.base.startsWith('http://127.')}
				<p class="text-xs text-muted">
					Recipients need to reach this app's address — a localhost link only works on this machine.
				</p>
			{/if}
		</div>
	{:else if open}
		<form class="space-y-3 rounded-md bg-surface p-3" onsubmit={make}>
			<label class="flex items-center justify-between gap-2 text-sm">
				<span>Expires</span>
				<select class="rounded-md border border-line bg-bg px-2 py-1 text-sm" bind:value={expires}>
					<option value="24">in a day</option>
					<option value="168">in a week</option>
					<option value="720">in 30 days</option>
					<!-- Last and explicit: "never" is the option to pick on purpose, not the default. -->
					<option value="never">never — revoke to end it</option>
				</select>
			</label>
			<label class="flex items-center justify-between gap-2 text-sm">
				<span>Download limit</span>
				<input
					type="number"
					min="1"
					class="w-24 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={maxDownloads}
					placeholder="none"
				/>
			</label>
			<label class="flex items-center justify-between gap-2 text-sm">
				<span>Passcode</span>
				<input
					class="w-40 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={passcode}
					placeholder="optional"
					autocomplete="off"
				/>
			</label>
			<label class="flex items-center gap-2 text-sm">
				<input
					type="checkbox"
					class="rounded border-line text-accent"
					bind:checked={allowOriginal}
				/>
				<span>
					Allow the original
					<span class="text-xs text-muted">— otherwise the web rendition</span>
				</span>
			</label>
			<div class="flex gap-2">
				<button
					type="submit"
					class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg"
				>
					Create link
				</button>
				<button type="button" class="text-xs underline" onclick={() => (open = false)}>
					Cancel
				</button>
			</div>
			{#if error}
				<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
			{/if}
		</form>
	{:else}
		<button
			type="button"
			class="rounded-md border border-line px-2.5 py-1 text-sm hover:bg-raised"
			onclick={() => (open = true)}
		>
			Share…
		</button>
	{/if}
</div>
