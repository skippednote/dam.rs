<script lang="ts">
	/**
	 * One round: the pictures, who was asked, and this caller's verdict.
	 *
	 * ## The pictures are the screen
	 *
	 * The task is looking at photographs. So the assets come first, as large as the column allows, and the
	 * paperwork — who asked, when, which pass this is — sits above them in one line. A review screen that led
	 * with a table of filenames would be asking somebody to approve things they had not seen, which is the one
	 * thing this feature must not do.
	 *
	 * ## Two verdicts, and neither of them is "pending"
	 *
	 * `pending` is where a reviewer starts, not something they can choose: offering it would let somebody
	 * un-decide, and a round whose reviewers can retract turns an approval into a snapshot of nothing. Changing
	 * a mind is a new round, which is what `supersedes` is for.
	 *
	 * ## A closed round is read-only and says why
	 *
	 * Not greyed-out buttons that fail on click. The server answers 409 with what to do instead — open a further
	 * round — and the screen offers exactly that.
	 *
	 * ## Specifics belong on the assets, not here
	 *
	 * The note is a covering remark. Anything about a particular picture goes in a comment on it, where it can
	 * be pinned to a region of the image — so each asset links to its own page rather than growing a comment box
	 * on this one.
	 */
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import {
		ApiError,
		cancelRound,
		decideRound,
		readRound,
		roundAssets,
		type Round,
		type RoundAsset
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	const id = $derived(page.params.id ?? '');

	let round = $state<Round | null>(null);
	let assets = $state<RoundAsset[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');
	let note = $state('');
	let busy = $state(false);
	/** The inline confirm for withdrawing. Not a `confirm()`: it blocks the automation driving the page. */
	let confirming = $state(false);

	async function load() {
		try {
			// Sequential, not parallel: the assets call re-checks the same visibility the round call does, and if
			// the round is a 404 there is nothing to draw — firing both would surface whichever failed first and
			// make a 404 look like a delivery problem.
			round = await readRound(id);
			assets = await roundAssets(id);
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 404
					? 'No such round — or it contains assets you cannot see, in which case it is not yours to review.'
					: caught instanceof ApiError
						? caught.message
						: 'Could not read the round.';
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		if (!session.connected) {
			loading = false;
			return;
		}
		void load();
	});

	const closed = $derived(round !== null && round.closed_at !== null);

	async function decide(verdict: 'approved' | 'changes_requested') {
		busy = true;
		error = '';
		notice = '';
		try {
			// The server returns the round's new outcome, so it is taken from the reply rather than guessed:
			// my approval does not make a round approved if somebody else asked for changes.
			round = await decideRound(id, verdict, note.trim());
			note = '';
			notice =
				round.outcome === 'open'
					? 'Recorded. The round stays open for the others.'
					: round.outcome === 'approved'
						? 'Recorded. Everybody has approved, so the round is closed.'
						: 'Recorded. The round is closed asking for changes.';
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'You are not a reviewer on this round, so there is no verdict of yours to record.'
					: caught instanceof ApiError && caught.status === 409
						? 'This round is closed. A further review is a new round over the same assets.'
						: caught instanceof Error
							? caught.message
							: 'Could not record that.';
		} finally {
			busy = false;
		}
	}

	async function withdraw() {
		busy = true;
		error = '';
		try {
			round = await cancelRound(id);
			confirming = false;
			notice = 'Withdrawn. The verdicts already given are kept.';
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Withdrawing a round is the requester’s act; this key does not hold Manage.'
					: caught instanceof Error
						? caught.message
						: 'Could not withdraw it.';
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>{round ? round.title : 'Round'} · damrs</title></svelte:head>

<div class="mx-auto max-w-4xl space-y-6 p-8">
	<p class="text-xs">
		<a href={resolve('/proofing')} class="text-muted underline hover:text-fg">Proofing</a>
	</p>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>

	{#if !session.connected}
		<p class="text-sm text-muted">Not connected.</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if round}
		<header class="space-y-2">
			<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
				<h1 class="text-2xl font-semibold tracking-tight">{round.title}</h1>
				<span
					class="rounded border px-1.5 py-0.5 text-xs whitespace-nowrap {round.outcome ===
					'approved'
						? 'border-state-rights-allowed text-state-rights-allowed-fg'
						: round.outcome === 'changes_requested'
							? 'border-state-rights-expiring text-state-rights-expiring-fg'
							: round.outcome === 'cancelled'
								? 'border-line text-muted'
								: 'border-line'}"
					data-testid="outcome"
				>
					{round.outcome === 'changes_requested' ? 'changes requested' : round.outcome}
				</span>
			</div>
			<p class="flex flex-wrap gap-x-3 gap-y-1 text-xs text-muted">
				<span>round {round.number}</span>
				{#if round.requested_by}<span>asked by {round.requested_by.name}</span>{/if}
				<span>opened {new Date(round.created_at).toLocaleString()}</span>
				{#if round.due_at}<span>due {new Date(round.due_at).toLocaleDateString()}</span>{/if}
				{#if round.closed_at}<span>closed {new Date(round.closed_at).toLocaleString()}</span>{/if}
			</p>
			{#if round.brief}
				<p class="max-w-2xl text-sm">{round.brief}</p>
			{/if}
			{#if round.supersedes}
				<p class="text-xs text-muted">
					A further pass over
					<a
						href={resolve('/proofing/[id]', { id: round.supersedes })}
						class="underline decoration-line hover:decoration-fg">the previous round</a
					>.
				</p>
			{/if}
		</header>

		<section class="space-y-3" aria-labelledby="assets">
			<h2 id="assets" class="text-sm font-semibold tracking-tight">
				{assets.length}
				{assets.length === 1 ? 'asset' : 'assets'}
				<span class="font-normal text-muted">· in the order they were sent</span>
			</h2>
			{#if assets.length === 0}
				<p class="text-sm text-muted">
					Every asset in this round has since been deleted. The verdicts stand as a record of what was
					decided.
				</p>
			{:else}
				<ul class="grid grid-cols-2 gap-3 sm:grid-cols-3 md:grid-cols-4" data-testid="round-assets">
					{#each assets as asset (asset.asset_id)}
						<li class="space-y-1">
							<a
								href={resolve('/assets')}
								class="block rounded-md border border-line bg-surface p-1"
								data-testid="asset-{asset.asset_id}"
							>
								{#if asset.thumbnail_url}
									<img
										src={asset.thumbnail_url}
										alt={asset.filename}
										class="h-32 w-full rounded object-contain"
										loading="lazy"
									/>
								{:else}
									<!-- Absent is not an error: nothing has been rendered yet. Saying so beats a
									     broken image icon, which reads as a failure of this page. -->
									<span
										class="flex h-32 w-full items-center justify-center rounded text-center text-xs text-muted"
										>no preview yet</span
									>
								{/if}
							</a>
							<p class="truncate text-xs text-muted" title={asset.filename}>{asset.filename}</p>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="space-y-2" aria-labelledby="reviewers">
			<h2 id="reviewers" class="text-sm font-semibold tracking-tight">Reviewers</h2>
			<ul class="space-y-1">
				{#each round.reviewers as reviewer (reviewer.person.id)}
					<li class="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-sm">
						<span class:text-muted={reviewer.verdict === 'pending'}>{reviewer.person.name}</span>
						<span
							class="text-xs {reviewer.verdict === 'approved'
								? 'text-state-rights-allowed-fg'
								: reviewer.verdict === 'changes_requested'
									? 'text-state-rights-expiring-fg'
									: 'text-muted'}"
						>
							{reviewer.verdict === 'pending'
								? 'has not answered'
								: reviewer.verdict === 'approved'
									? 'approved'
									: 'asked for changes'}
						</span>
						{#if reviewer.decided_at}
							<span class="text-xs text-muted">
								{new Date(reviewer.decided_at).toLocaleString()}
							</span>
						{/if}
						{#if reviewer.note}
							<span class="w-full text-xs text-muted">“{reviewer.note}”</span>
						{/if}
					</li>
				{/each}
			</ul>
		</section>

		<section class="space-y-3 rounded-md border border-line p-3" aria-labelledby="verdict">
			<h2 id="verdict" class="text-sm font-semibold tracking-tight">Your verdict</h2>
			{#if closed}
				<p class="max-w-2xl text-sm text-muted">
					This round is closed, so no further verdict can be recorded on it. A further review is a new
					round over the same assets — select them on the grid and send a second pass, which will
					point back at this one.
				</p>
			{:else}
				<label class="block space-y-1">
					<span class="text-xs text-muted">
						A covering note, if you have one. Anything about a particular picture belongs in a comment
						on it, where it can be pinned to the part you mean.
					</span>
					<textarea
						class="w-full rounded-md border border-line bg-bg px-2 py-1 text-sm"
						rows="2"
						bind:value={note}
						data-testid="note"
					></textarea>
				</label>
				<div class="flex flex-wrap gap-2 text-sm">
					<button
						type="button"
						class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg disabled:opacity-50"
						disabled={busy}
						onclick={() => decide('approved')}
						data-testid="approve"
					>
						Approve
					</button>
					<button
						type="button"
						class="rounded-md border border-state-rights-expiring px-2.5 py-1 text-state-rights-expiring-fg disabled:opacity-50"
						disabled={busy}
						onclick={() => decide('changes_requested')}
						data-testid="request-changes"
					>
						Request changes
					</button>
				</div>
				{#if notice}
					<p class="text-xs text-muted">{notice}</p>
				{/if}
			{/if}
		</section>

		{#if !closed}
			<section class="space-y-2" aria-labelledby="withdraw">
				<h2 id="withdraw" class="sr-only">Withdraw</h2>
				{#if confirming}
					<p class="text-sm">
						Withdraw this round? The verdicts already given are kept and still shown; nobody will be
						asked for a further one.
					</p>
					<div class="flex gap-2 text-sm">
						<button
							type="button"
							class="rounded-md border border-state-rights-denied px-2.5 py-1 text-state-rights-denied-fg disabled:opacity-50"
							disabled={busy}
							onclick={() => withdraw()}
							data-testid="withdraw-confirm"
						>
							Withdraw it
						</button>
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
							onclick={() => (confirming = false)}
						>
							Keep it open
						</button>
					</div>
				{:else}
					<button
						type="button"
						class="text-xs text-muted underline hover:text-fg"
						onclick={() => (confirming = true)}
						data-testid="withdraw"
					>
						Withdraw this round
					</button>
				{/if}
			</section>
		{/if}
	{/if}
</div>
