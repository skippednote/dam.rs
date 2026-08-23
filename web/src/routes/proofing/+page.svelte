<script lang="ts">
	/**
	 * Proofing rounds: who has been asked to look at what, and who has answered.
	 *
	 * ## What is waiting on *you* comes first, and is a different question from what you can see
	 *
	 * An administrator can see every round in the tenant and is a reviewer on almost none of them. So the page
	 * leads with the rounds where this caller's own verdict is still pending — the reason a reviewer opens this
	 * screen at all — and the full list follows underneath. Merging them into one list ordered by date would
	 * bury two things somebody is blocking on under forty they are only watching.
	 *
	 * ## The outcome is derived and the page says so
	 *
	 * `changes_requested` wins over any number of approvals, and there is no stored state to disagree with the
	 * verdicts. That is worth stating on the page because "approved" on a round where one person asked for
	 * changes is the failure mode people expect from review tools, and it cannot happen here.
	 *
	 * ## Every reviewer is named, always
	 *
	 * Including the ones who have not answered. "Waiting on Bob" is the entire value of a review list, and a
	 * summary count — "2 of 3" — is the version of this screen that makes somebody open a spreadsheet.
	 *
	 * ## The list is per-caller
	 *
	 * A round is visible only when *all* of its assets are, so two people legitimately see different lists. The
	 * page says this rather than leaving it to be discovered as a bug report.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { ApiError, listRounds, myRounds, type Round } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let all = $state<Round[]>([]);
	let mine = $state<Round[]>([]);
	let loading = $state(true);
	let error = $state('');

	onMount(async () => {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			// Both, in parallel: they answer different questions and neither is derivable from the other — a
			// round can be waiting on me and a round I can see are overlapping sets, not nested ones.
			[all, mine] = await Promise.all([listRounds(), myRounds()]);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the rounds.';
		} finally {
			loading = false;
		}
	});

	const open = $derived(all.filter((round) => round.outcome === 'open'));
</script>

<svelte:head><title>Proofing · damrs</title></svelte:head>

{#snippet outcome(round: Round)}
	<!--
		A word, and a colour that carries no meaning on its own.

		`changes_requested` uses the expiring colour rather than the denied one: somebody asking for a tighter
		crop is not a refusal, and painting it red next to a real rights denial elsewhere in the application
		would flatten two very different things into one alarm.
	-->
	<span
		class="rounded border px-1.5 py-0.5 text-xs whitespace-nowrap {round.outcome === 'approved'
			? 'border-state-rights-allowed text-state-rights-allowed-fg'
			: round.outcome === 'changes_requested'
				? 'border-state-rights-expiring text-state-rights-expiring-fg'
				: round.outcome === 'cancelled'
					? 'border-line text-muted'
					: 'border-line'}"
		data-testid="outcome-{round.id}"
	>
		{round.outcome === 'changes_requested'
			? 'changes requested'
			: round.outcome === 'open'
				? 'open'
				: round.outcome}
	</span>
{/snippet}

{#snippet row(round: Round)}
	<li class="space-y-1.5 rounded-md border border-line p-3">
		<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
			<h3 class="text-sm font-semibold tracking-tight">
				<a
					href={resolve('/proofing/[id]', { id: round.id })}
					class="underline decoration-line hover:decoration-fg"
				>
					{round.title}
				</a>
			</h3>
			{@render outcome(round)}
			{#if round.number > 1}
				<!-- Which pass this is. A second round exists because a first one asked for changes, and that
				     history is the difference between "approved" and "approved eventually". -->
				<span class="text-xs text-muted">round {round.number}</span>
			{/if}
			<span class="text-xs text-muted">
				{round.asset_count}
				{round.asset_count === 1 ? 'asset' : 'assets'}
			</span>
			{#if round.requested_by}
				<span class="text-xs text-muted">asked by {round.requested_by.name}</span>
			{/if}
			{#if round.due_at}
				<span class="text-xs text-muted">due {new Date(round.due_at).toLocaleDateString()}</span>
			{/if}
		</div>

		{#if round.brief}
			<p class="max-w-2xl text-xs text-muted">{round.brief}</p>
		{/if}

		<!-- Named, all of them, answered or not. -->
		<ul class="flex flex-wrap gap-x-3 gap-y-0.5">
			{#each round.reviewers as reviewer (reviewer.person.id)}
				<li class="text-xs">
					<span class:text-muted={reviewer.verdict === 'pending'}>{reviewer.person.name}</span>
					<span
						class="ml-1 {reviewer.verdict === 'approved'
							? 'text-state-rights-allowed-fg'
							: reviewer.verdict === 'changes_requested'
								? 'text-state-rights-expiring-fg'
								: 'text-muted'}"
					>
						{reviewer.verdict === 'pending'
							? 'waiting'
							: reviewer.verdict === 'approved'
								? 'approved'
								: 'changes'}
					</span>
				</li>
			{/each}
		</ul>
	</li>
{/snippet}

<div class="mx-auto max-w-4xl space-y-8 p-8">
	<header class="space-y-1">
		<h1 class="text-2xl font-semibold tracking-tight">Proofing</h1>
		<p class="max-w-2xl text-sm text-muted">
			Rounds of review over a fixed set of assets. A round records that people agreed — it gates
			nothing, so approving one does not publish anything and an unapproved asset is not blocked. Each
			round's outcome is worked out from its verdicts, which is why one person asking for changes shows
			as <em>changes requested</em> however many others approved.
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
	{:else}
		<section class="space-y-3" aria-labelledby="waiting">
			<h2 id="waiting" class="text-sm font-semibold tracking-tight">Waiting on you</h2>
			{#if mine.length === 0}
				<p class="max-w-2xl text-sm text-muted" data-testid="none-waiting">
					Nothing is waiting on your verdict. Rounds you can see but were not asked about are below.
				</p>
			{:else}
				<ul class="space-y-2" data-testid="mine">
					{#each mine as round (round.id)}
						{@render row(round)}
					{/each}
				</ul>
			{/if}
		</section>

		<section class="space-y-3" aria-labelledby="every">
			<h2 id="every" class="text-sm font-semibold tracking-tight">
				All rounds you can see
				{#if all.length > 0}
					<span class="font-normal text-muted"
						>· {open.length} open of {all.length}</span
					>
				{/if}
			</h2>
			<p class="max-w-2xl text-xs text-muted">
				A round appears here only when you can see <em>every</em> asset in it — a partly visible round
				is not shown at all, because showing some of the pictures would be asking somebody to judge a
				set they never saw.
			</p>
			{#if all.length === 0}
				<p class="text-sm text-muted">
					No rounds. Select assets on the grid and use <em>Review…</em> to ask somebody to look at
					them.
				</p>
			{:else}
				<ul class="space-y-2" data-testid="all">
					{#each all as round (round.id)}
						{@render row(round)}
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</div>
