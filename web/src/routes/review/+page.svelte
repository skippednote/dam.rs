<script lang="ts">
	/**
	 * The review queue: what a model proposed, and nobody has decided about yet.
	 *
	 * ## Why this screen exists at all
	 *
	 * Because a suggestion is not a tag. Every LLM tag lands `suggested` whatever confidence the model claimed,
	 * so without somewhere to accept them they would sit in the database forever, invisible to search and
	 * useless. This is the other half of that decision.
	 *
	 * ## Rejections matter as much as acceptances
	 *
	 * `tag_feedback` is the training set, and its own schema note says an edit history that loses the rejections
	 * loses the signal that matters most. So *No* is a first-class button here, not a dismissal — it is recorded
	 * with the same weight as *Yes*.
	 *
	 * ## Agreement, not confidence, leads
	 *
	 * The server orders by how many independent generators proposed a tag before it orders by what any of them
	 * claimed about it. A self-reported 0.95 is worth less than two models agreeing, and the badge says which
	 * you are looking at.
	 *
	 * ## What a model wrote is shown, not hidden
	 *
	 * The machine-written fields sit beside the tags with the model that produced them, because that marking is
	 * the disclosure (Article 50) rather than an implementation detail. An asset whose description a model wrote
	 * says so, here and on the asset itself.
	 */
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import {
		decideTag,
		readReviewQueue,
		ApiError,
		type ReviewRow,
		type SuggestedTag
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';
	import { onMount } from 'svelte';

	let queue = $state<ReviewRow[]>([]);
	let problem = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let loading = $state(true);
	let deciding = $state<string | null>(null);

	const pending = $derived(queue.reduce((total, row) => total + row.suggested.length, 0));

	async function load() {
		loading = true;
		problem = null;
		try {
			queue = await readReviewQueue();
		} catch (caught) {
			problem =
				caught instanceof ApiError
					? caught.status === 403
						? 'This key may not review suggestions. Reviewing needs manage access.'
						: caught.message
					: 'Could not reach the API.';
		} finally {
			loading = false;
		}
	}

	/**
	 * Opens the grid, searched for one asset.
	 *
	 * The same shape the grid's own search uses: a URL derived from `page.url`, so whatever base path the app is
	 * served under is already in it. `resolve` cannot express a query string, which is what the rule below is
	 * about — it exists to catch a hand-written internal path, not this.
	 */
	function open(filename: string) {
		const url = new URL(resolve('/assets'), page.url);
		url.searchParams.set('q', filename);
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		void goto(url);
	}

	async function decide(row: ReviewRow, tag: SuggestedTag, accept: boolean) {
		deciding = `${row.asset_id}:${tag.term_id}`;
		problem = null;
		notice = null;
		try {
			await decideTag(row.asset_id, tag.term_id, accept);
			// Removed locally rather than by reloading: a reviewer working down a long queue should not have it
			// reorder under them after every click.
			row.suggested = row.suggested.filter((other) => other.term_id !== tag.term_id);
			queue = queue.filter((other) => other.suggested.length > 0 || other.fields.length > 0);
			notice = `${accept ? 'Confirmed' : 'Rejected'} ${tag.label}.`;
		} catch (caught) {
			problem =
				caught instanceof ApiError && caught.status === 404
					? 'Somebody else already decided that one.'
					: 'Could not record the decision.';
		} finally {
			deciding = null;
		}
	}

	function strength(tag: SuggestedTag): string {
		if (tag.votes > 1) return `${tag.votes} generators agree`;
		return tag.confidence === null || tag.confidence === undefined
			? 'one generator'
			: `claimed ${Math.round(tag.confidence * 100)}%`;
	}

	onMount(() => {
		if (session.connected) void load();
	});
</script>

<div class="mx-auto max-w-4xl space-y-6 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Review</h1>
		<p class="mt-1 text-sm text-muted">
			What a model proposed. Nothing here counts until you say so — and saying no is as useful as
			saying yes, because both train the next round.
			<a class="underline" href={resolve('/settings/ai')}>Settings</a>
		</p>
	</div>

	{#if !session.connected}
		<p class="rounded-md bg-surface p-3 text-sm">
			Not connected. Add an API key on the
			<a class="underline" href={resolve('/settings')}>connection</a> screen first.
		</p>
	{:else}
		{#if problem}
			<p
				role="alert"
				class="rounded-md bg-state-rights-denied/18 p-3 text-sm text-state-rights-denied-fg"
			>
				{problem}
			</p>
		{/if}
		{#if notice}
			<p
				role="status"
				class="rounded-md bg-state-rights-allowed/18 p-3 text-sm text-state-rights-allowed-fg"
			>
				{notice}
			</p>
		{/if}

		{#if loading}
			<p class="text-sm text-muted">Loading…</p>
		{:else if queue.length === 0}
			<p class="rounded-md bg-surface p-4 text-sm">
				Nothing to review. Either no model has run yet, or somebody has been through it already.
			</p>
		{:else}
			<p class="text-sm text-muted">
				{pending} suggestion{pending === 1 ? '' : 's'} across {queue.length} asset{queue.length ===
				1
					? ''
					: 's'}.
			</p>

			<ul class="space-y-4">
				{#each queue as row (row.asset_id)}
					<li class="space-y-3 rounded-md border border-line p-4">
						<div class="flex flex-wrap items-baseline gap-2">
							<!--
								The grid rather than a per-asset route: asset detail is a panel on /assets, so
								searching for the filename is how somebody gets there from here.
							-->
							<!--
								The href is the plain grid, so this works with no JavaScript; the click adds the
								filename as a search so a reviewer lands on the asset rather than on everything.
								Asset detail is a panel on /assets, not a route of its own, which is why there is
								nothing more specific to link to.
							-->
							<a
								class="font-medium underline"
								href={resolve('/assets')}
								onclick={(event) => {
									event.preventDefault();
									open(row.filename);
								}}
							>
								{row.filename}
							</a>
							<span class="font-mono text-xs text-muted">{row.mime}</span>
						</div>

						{#if row.fields.length > 0}
							<div class="space-y-2">
								{#each row.fields as field (field.key)}
									<div class="rounded-md bg-surface p-2 text-sm">
										<div class="flex flex-wrap items-baseline gap-2">
											<span class="text-xs font-semibold tracking-wide text-muted uppercase">
												{field.key}
											</span>
											<span class="font-mono text-xs text-muted">{field.model}</span>
											{#if field.reviewed}
												<span class="text-xs text-muted">reviewed</span>
											{/if}
										</div>
										<p class="mt-1">{field.value}</p>
									</div>
								{/each}
							</div>
						{/if}

						{#if row.suggested.length > 0}
							<ul class="flex flex-wrap gap-2">
								{#each row.suggested as tag (tag.term_id)}
									<li class="flex items-center gap-2 rounded-md border border-line px-2 py-1">
										<span class="text-sm">{tag.label}</span>
										<span class="text-xs text-muted">{strength(tag)}</span>
										<!--
											The visible label is Yes/No because the tag is right there; the
											accessible name names the tag, because a screen reader reaching the
											fortieth button on the page has no such context.
										-->
										<button
											type="button"
											class="rounded bg-accent px-2 py-0.5 text-xs font-medium text-accent-fg disabled:opacity-50"
											aria-label={`Confirm ${tag.label} on ${row.filename}`}
											disabled={deciding === `${row.asset_id}:${tag.term_id}`}
											onclick={() => decide(row, tag, true)}
										>
											Yes
										</button>
										<button
											type="button"
											class="rounded border border-line px-2 py-0.5 text-xs disabled:opacity-50"
											aria-label={`Reject ${tag.label} on ${row.filename}`}
											disabled={deciding === `${row.asset_id}:${tag.term_id}`}
											onclick={() => decide(row, tag, false)}
										>
											No
										</button>
									</li>
								{/each}
							</ul>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</div>
