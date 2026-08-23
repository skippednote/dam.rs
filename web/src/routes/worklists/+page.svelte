<script lang="ts">
	/**
	 * The worklists: what the library is missing, and what to do about each one.
	 *
	 * ## Sorted by consequence, not by size
	 *
	 * Three of these are rights exposure — an asset served past its expiry date, a licence about to lapse,
	 * paperwork that was never recorded — and the rest are tidiness. A list ordered by count would bury the
	 * three under a thousand missing captions, which is exactly backwards: the server marks them `urgent` and
	 * they lead.
	 *
	 * ## Every count says whose it is
	 *
	 * These are per-caller, so two people see different numbers here. Said out loud on the page rather than
	 * left to be discovered, because "Ada says there are 40 and I see 12" is otherwise a bug report.
	 *
	 * ## An empty list is the good outcome, and reads like one
	 *
	 * A zero row stays visible but goes quiet. Hiding it would make the page shorter and leave nobody able to
	 * tell "nothing expired" from "we do not check for expiry".
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { ApiError, listWorklists, type Worklist } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let lists = $state<Worklist[]>([]);
	let loading = $state(true);
	let error = $state('');

	onMount(async () => {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			lists = await listWorklists();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the worklists.';
		} finally {
			loading = false;
		}
	});

	const outstanding = $derived(lists.reduce((sum, one) => sum + one.count, 0));
</script>

<svelte:head><title>Worklists · damrs</title></svelte:head>

<div class="mx-auto max-w-4xl space-y-6 p-8">
	<header class="space-y-1">
		<h1 class="text-2xl font-semibold tracking-tight">Worklists</h1>
		<p class="max-w-2xl text-sm text-muted">
			Gaps in the library, computed from what is already recorded — so an asset leaves a list the
			moment somebody fixes the thing. Every number is what <em>you</em> can see; somebody with wider
			access sees larger ones.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if outstanding === 0}
		<p class="text-sm text-muted">
			Nothing outstanding on any list. Every asset you can see is filed, described, licensed and
			within its dates.
		</p>
	{/if}

	<ul class="space-y-2">
		{#each lists as list (list.key)}
			<li
				class="flex flex-wrap items-baseline gap-x-3 gap-y-1 rounded-md border p-3 {list.urgent &&
				list.count > 0
					? 'border-state-rights-denied'
					: 'border-line'}"
			>
				<h2 class="text-sm font-semibold tracking-tight">
					{#if list.count > 0}
						<a
							href={resolve('/worklists/[key]', { key: list.key })}
							class="underline decoration-line hover:decoration-fg"
						>
							{list.label}
						</a>
					{:else}
						<!-- No link when there is nothing to open: a link to an empty page is a dead end that
						     looks like a destination. -->
						<span class="text-muted">{list.label}</span>
					{/if}
				</h2>
				<span
					class="text-lg font-semibold tabular-nums {list.count === 0 ? 'text-muted' : ''}"
					data-testid="count-{list.key}"
				>
					{list.count}
				</span>
				{#if list.urgent && list.count > 0}
					<span
						class="rounded border border-state-rights-denied px-1.5 py-0.5 text-xs text-state-rights-denied-fg"
					>
						rights exposure
					</span>
				{/if}
				<p class="w-full text-xs text-muted">{list.explanation}</p>
			</li>
		{/each}
	</ul>
</div>
