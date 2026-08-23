<script lang="ts">
	/**
	 * The landing page: what is here, what has happened, and what needs doing.
	 *
	 * ## Every number is this caller's
	 *
	 * §7 says a count is a disclosure, and a dashboard is almost entirely counts. A scoped reader sees their own
	 * totals — showing the library's with their own results beneath would tell them exactly how much they cannot
	 * reach.
	 *
	 * ## A count with nothing to do about it is a decoration
	 *
	 * Each figure that *can* be acted on is a link into the search that produced it. "Assets with no metadata" is
	 * deliberately not one: there is no selector for it yet, and a link that led somewhere else would answer a
	 * different question while looking like it answered this one.
	 *
	 * ## The feed says what happened, never the private part of it
	 *
	 * The server decides that — a share event carries no token and a comment event carries no words — and this page
	 * renders what it is given. Worth knowing while reading the phrasing below: there is nothing here to redact,
	 * because nothing sensitive arrives.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { ApiError, loadDashboard, type Dashboard } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';
	// The phrasing is shared with the per-asset history panel (Q.10): same sentence, different scope. See
	// `$lib/activity` on why there is only one copy.
	import { phrase, when } from '$lib/activity';

	let data = $state<Dashboard | null>(null);
	let error = $state('');
	let loading = $state(true);

	async function load() {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			data = await loadDashboard();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load the dashboard.';
		} finally {
			loading = false;
		}
	}

	onMount(load);
</script>

<div class="mx-auto max-w-5xl space-y-8 p-8">
	<div>
		<p class="mb-2 text-[10px] font-semibold tracking-[0.16em] text-accent uppercase">
			Library overview
		</p>
		<h1 class="text-3xl font-semibold tracking-[-0.03em]">What needs attention</h1>
		<p class="mt-1 text-sm text-muted">
			Your view of the library, its recent movement, and the work waiting behind it.
		</p>
		<p class="mt-1 text-xs text-muted">
			Counts reflect what <em>you</em> can see. Somebody with wider access may see larger numbers.
		</p>
	</div>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-3 text-sm text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if data}
		<!-- The counts. Each one that can be acted on is a link; see the component docs on the one that is not. -->
		<section aria-label="Library at a glance">
			<h2 class="sr-only">Library at a glance</h2>
			<!--
				`div` wrappers inside the `dl`. A `dl` may contain only `dt`/`dd` groups or `div`s wrapping them, so
				the first version — an `a` as a direct child, to make a whole tile clickable — was invalid structure
				and axe said so. The link lives inside the `dd` instead, which is also the better reading order: the
				label, then the number you can act on.
			-->
			<dl
				class="grid grid-cols-2 gap-px overflow-hidden rounded-md border border-line bg-line sm:grid-cols-3 lg:grid-cols-5"
			>
				<div class="bg-bg p-4" data-testid="count-assets">
					<dt class="text-xs text-muted">Assets you can see</dt>
					<dd class="mt-1 text-2xl font-semibold tabular-nums">
						<a href={resolve('/assets')} class="underline decoration-line hover:decoration-fg">
							{data.counts.assets}
						</a>
					</dd>
				</div>
				<div class="bg-bg p-4">
					<dt class="text-xs text-muted">Uploaded this week</dt>
					<dd class="mt-1 text-2xl font-semibold tabular-nums">
						{data.counts.uploads_this_week}
					</dd>
				</div>
				<div class="bg-bg p-4">
					<dt class="text-xs text-muted">Downloaded this week</dt>
					<dd class="mt-1 text-2xl font-semibold tabular-nums">
						{data.counts.downloads_this_week}
					</dd>
				</div>
				<div class="bg-bg p-4">
					<dt class="text-xs text-muted">Comments this week</dt>
					<dd class="mt-1 text-2xl font-semibold tabular-nums">
						{data.counts.comments_this_week}
					</dd>
				</div>
				<div class="bg-bg p-4" data-testid="count-undescribed">
					<dt class="text-xs text-muted">Nothing written about them</dt>
					<dd class="mt-1 text-2xl font-semibold tabular-nums">
						{data.counts.without_metadata}
					</dd>
				</div>
			</dl>
			<p class="mt-2 text-xs text-muted">
				"This week" is the last seven days, not since Monday — a dashboard that empties itself every
				Monday morning is one people stop opening.
			</p>
		</section>

		<section aria-label="Recent activity" class="space-y-2">
			<h2 class="text-lg font-semibold tracking-tight">Recent activity</h2>
			{#if data.activity.length === 0}
				<p class="text-sm text-muted">
					Nothing yet. Uploads, shares and comments appear here as they happen.
				</p>
			{:else}
				<ul class="divide-y divide-line rounded-md border border-line">
					{#each data.activity as entry (entry.id)}
						<li class="flex flex-wrap items-baseline gap-x-2 px-3 py-2 text-sm">
							<span>{phrase(entry)}</span>
							<time datetime={entry.occurred_at} class="text-xs text-muted">
								{when(entry.occurred_at)}
							</time>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section aria-label="Saved searches" class="space-y-2">
			<h2 class="text-lg font-semibold tracking-tight">Saved searches</h2>
			{#if data.spotlights.length === 0}
				<p class="text-sm text-muted">
					None yet. Save a search from the <a class="underline" href={resolve('/assets')}>Assets</a>
					page to keep it here.
				</p>
			{:else}
				<ul class="flex flex-wrap gap-2">
					{#each data.spotlights as spotlight (spotlight.id)}
						<li class="rounded-md border border-line px-3 py-2 text-sm">
							<span class="font-medium">{spotlight.name}</span>
							{#if spotlight.mine}
								<span class="ml-1 text-xs text-muted">· yours</span>
							{/if}
							{#if spotlight.cached_count !== null && spotlight.cached_count !== undefined}
								<!--
									Named as a cached figure, because it is computed for nobody in particular. Calling it
									"results" would tell a scoped reader how many assets exist beyond their reach.
								-->
								<span class="ml-1 text-xs text-muted tabular-nums">
									· {spotlight.cached_count} when last counted
								</span>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</div>
