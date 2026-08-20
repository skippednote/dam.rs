<script lang="ts">
	/**
	 * A portal: what somebody with no account sees when they open a link the library published (Q.14).
	 *
	 * ## The tenant's page, not ours
	 *
	 * A portal exists so an organisation can hand out part of its library under its own name. So the branding is
	 * the content: the title, the intro somebody wrote, the logo, and one accent colour that the buttons and the
	 * rules take. Everything else is deliberately plain — a page that competed with the assets would be the wrong
	 * page.
	 *
	 * The accent arrives as a hex colour from the API and is applied through a CSS custom property rather than a
	 * class, because it is *data*: there is no set of colours to choose from, and a class per tenant is not a
	 * thing that can exist.
	 *
	 * ## What the visitor is told, and what they are not
	 *
	 * Every asset in the set is listed, including the ones whose bytes cannot be handed over — with the reason,
	 * per asset. A portal that hid those would look like a smaller collection than the one that was published,
	 * and the sender would have no way to know. What the page never says is anything about the library *outside*
	 * the set: the search box narrows what was given, and finds nothing else.
	 *
	 * ## Refusals are in the visitor's terms
	 *
	 * "Expired" means ask for a new link; "passcode required" means look in the email. The server chooses those
	 * words — the same words the share portal uses, because a portal is a share.
	 */
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { ApiError, portalByKey, type PortalPage } from '$lib/api/client';
	import { legible } from '$lib/portal-colour';

	const key = page.params.key ?? '';

	let view = $state<PortalPage | null>(null);
	let needsPasscode = $state(false);
	let passcode = $state('');
	let query = $state('');
	let error = $state('');
	let loading = $state(true);

	/**
	 * The button's colours, derived from the tenant's accent so the pair always meets AA.
	 *
	 * The accent itself is data — whatever hex the organisation gave — and white text on it is often illegible:
	 * `#ff6600` manages 2.93:1, which the browser suite failed on. `legible` moves the *background* rather than
	 * rejecting the colour, so the button stays recognisably the brand and readable. See `$lib/portal-colour`.
	 */
	const button = $derived(legible(view?.accent ?? '#2563eb'));

	async function load() {
		loading = true;
		error = '';
		try {
			view = await portalByKey(key, {
				q: query.trim() || undefined,
				passcode: passcode || undefined
			});
			needsPasscode = false;
		} catch (caught) {
			if (caught instanceof ApiError && caught.status === 401) {
				needsPasscode = true;
				// Empty on the first ask: there is nothing wrong yet, and an error the visitor caused nothing
				// to deserve reads as a broken page.
				error = passcode ? caught.message : '';
			} else {
				error = caught instanceof ApiError ? caught.message : 'This portal could not be opened.';
				view = null;
			}
		} finally {
			loading = false;
		}
	}

	function search(event: SubmitEvent) {
		event.preventDefault();
		void load();
	}

	function bytes(n: number | null | undefined): string {
		if (n === null || n === undefined) return '';
		const units = ['B', 'kB', 'MB', 'GB'];
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
	<title>{view?.title ?? 'Portal'}</title>
</svelte:head>

<!--
	`style` rather than a class: the accent is a tenant's own colour, arriving as data. There is no fixed set to
	make classes from.
-->
<div
	class="mx-auto max-w-5xl space-y-8 p-6 sm:p-10"
	style={view
		? `--portal-accent: ${view.accent}; --portal-button: ${button.background}; --portal-ink: ${button.ink}`
		: undefined}
>
	{#if loading && !view}
		<p class="text-sm text-muted">Opening…</p>
	{:else if needsPasscode}
		<div class="mx-auto max-w-sm space-y-4">
			<h1 class="text-xl font-semibold tracking-tight">This portal needs a passcode</h1>
			<p class="text-sm text-muted">Whoever sent you the link will have included it.</p>
			<form class="space-y-3" onsubmit={search}>
				<label class="sr-only" for="passcode">Passcode</label>
				<input
					id="passcode"
					type="password"
					autocomplete="off"
					class="w-full rounded-md border border-line bg-bg px-3 py-2 text-sm"
					bind:value={passcode}
				/>
				<button
					type="submit"
					class="w-full rounded-md px-3 py-2 text-sm font-medium"
					style="background: var(--portal-button, #2563eb); color: var(--portal-ink, #ffffff)"
				>
					Open
				</button>
			</form>
			{#if error}
				<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
			{/if}
		</div>
	{:else if error}
		<div class="mx-auto max-w-md space-y-2 text-center">
			<h1 class="text-xl font-semibold tracking-tight">This portal is not available</h1>
			<p role="alert" class="text-sm text-muted">{error}</p>
		</div>
	{:else if view}
		<header class="space-y-3 border-b border-line pb-6">
			{#if view.logo_url}
				<!--
					A logo is an asset, delivered through the same signed chokepoint as everything else — which is
					why it can be missing, and why a missing one is simply absent rather than a broken image.
				-->
				<img src={view.logo_url} alt="" class="h-12 w-auto" />
			{/if}
			<h1 class="text-2xl font-semibold tracking-tight">{view.title}</h1>
			{#if view.intro}
				<p class="max-w-2xl text-sm text-muted">{view.intro}</p>
			{/if}
			<div class="flex flex-wrap items-center gap-3 text-xs text-muted">
				<span>{view.total} asset{view.total === 1 ? '' : 's'}</span>
				{#if view.downloads_remaining !== null && view.downloads_remaining !== undefined}
					<span
						>{view.downloads_remaining} download{view.downloads_remaining === 1 ? '' : 's'} left</span
					>
				{/if}
				{#if view.expires_at}
					<span>available until {new Date(view.expires_at).toLocaleDateString()}</span>
				{/if}
			</div>
		</header>

		{#if view.allow_search}
			<form class="flex items-center gap-2" onsubmit={search} role="search">
				<label class="sr-only" for="q">Search this portal</label>
				<input
					id="q"
					class="min-w-0 flex-1 rounded-md border border-line bg-bg px-3 py-1.5 text-sm"
					bind:value={query}
					placeholder="Search these assets"
				/>
				<button
					type="submit"
					class="rounded-md px-3 py-1.5 text-sm font-medium"
					style="background: var(--portal-button, #2563eb); color: var(--portal-ink, #ffffff)"
					disabled={loading}
				>
					{loading ? 'Searching…' : 'Search'}
				</button>
			</form>
		{/if}

		{#if view.items.length === 0}
			<p class="rounded-md bg-surface p-4 text-sm">
				{#if view.query}
					Nothing here matches “{view.query}”.
				{:else}
					This portal is empty at the moment.
				{/if}
			</p>
		{:else}
			<ul class="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-4">
				{#each view.items as item (item.asset_id)}
					<li class="space-y-2">
						<div
							class="flex aspect-square items-center justify-center overflow-hidden rounded-md border border-line bg-surface"
						>
							{#if item.preview_url}
								<img
									src={item.preview_url}
									alt={item.filename}
									class="h-full w-full object-cover"
									loading="lazy"
								/>
							{:else}
								<!--
									Named, with the reason. An asset the sender published and the licence will not
									release is a fact both of them need — hiding it would leave the sender thinking
									they had shared something they had not.
								-->
								<p class="p-3 text-center text-xs text-muted">{item.preview_unavailable}</p>
							{/if}
						</div>
						<p class="truncate text-sm" title={item.filename}>{item.filename}</p>
						<p class="text-xs text-muted">{bytes(item.bytes)}</p>
					</li>
				{/each}
			</ul>
			{#if view.total > view.items.length}
				<p class="text-xs text-muted">
					Showing the first {view.items.length} of {view.total}. Search to narrow them down.
				</p>
			{/if}
		{/if}
	{/if}
</div>
