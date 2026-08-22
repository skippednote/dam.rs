<!--
	Orders: what I asked for, and what is waiting for me to decide (Q.13c).

	## Two lists, because they are two questions

	"My orders" is a history — newest first, and it is where a requester learns their answer. "Waiting for a
	decision" is a queue — oldest first, because a queue is worked through and the longest wait is the next thing
	to do. Showing them as one list would sort somebody's own history in with other people's requests.

	## The queue is absent, not empty, for somebody who cannot decide

	A reader has no business seeing what colleagues have asked for. The server refuses it with a 403, and this
	renders nothing rather than an error: being unable to approve is not a fault.

	## An approval is not a handover

	An approved order says so, and says when the window closes. What it does *not* say is "here are your files" —
	the pickup is a share created by fulfilment, which is a separate slice, and claiming otherwise would be the
	interface promising something the system has not done.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import {
		ApiError,
		decideOrder,
		loadMyOrders,
		loadOrderQueue,
		reissuePickup,
		type Order
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let mine = $state<Order[]>([]);
	let queue = $state<Order[] | null>(null);
	let error = $state('');
	let notice = $state('');
	let loading = $state(true);
	/** The order being decided, so one row at a time carries the note field. */
	let deciding = $state<string | null>(null);
	let note = $state('');
	/**
	 * Pickup links, by order id, for the ones minted in this session.
	 *
	 * Held in memory and nowhere else: a share token is stored as a digest, so this is the only readable copy and
	 * writing it to storage would turn a lost link into a leaked one. Gone on reload, which is what re-issuing is
	 * for.
	 */
	let pickups = $state<Record<string, string>>({});

	async function load() {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			// `allSettled`, because the queue is a 403 for most people and that must not empty their own list.
			const [own, waiting] = await Promise.allSettled([loadMyOrders(), loadOrderQueue()]);
			if (own.status === 'fulfilled') mine = own.value;
			else throw own.reason;
			queue = waiting.status === 'fulfilled' ? waiting.value : null;
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load orders.';
		} finally {
			loading = false;
		}
	}

	function decide(order: Order, decision: 'approve' | 'reject' | 'cancel') {
		error = '';
		notice = '';
		void (async () => {
			try {
				const updated = await decideOrder(order.id, decision, note || undefined);
				if (updated.pickup_url) pickups[updated.id] = updated.pickup_url;
				notice = `${updated.reference} is ${updated.state}.`;
				deciding = null;
				note = '';
				await load();
			} catch (caught) {
				// The server's own sentence: "already approved", or how many assets are outside your scope. Both
				// are things the person can act on.
				error = caught instanceof ApiError ? caught.message : 'That decision did not go through.';
			}
		})();
	}

	/**
	 * Where the metadata export lives.
	 *
	 * A plain authenticated GET the browser can follow, rather than a `fetch` that would pull a spreadsheet
	 * through the page only to hand it back. The base comes from the session, as every other call's does.
	 */
	function metadataHref(order: Order): string {
		return `${session.base}/orders/${order.id}/metadata.csv`;
	}

	function reissue(order: Order) {
		error = '';
		notice = '';
		void (async () => {
			try {
				const updated = await reissuePickup(order.id);
				if (updated.pickup_url) pickups[updated.id] = updated.pickup_url;
				notice = `${updated.reference} has a new pickup link. The previous one no longer works.`;
				await load();
			} catch (caught) {
				error = caught instanceof ApiError ? caught.message : 'That link could not be re-issued.';
			}
		})();
	}

	function when(iso: string | null | undefined): string {
		return iso ? new Date(iso).toLocaleDateString() : '';
	}

	onMount(load);
</script>

<div class="mx-auto max-w-5xl space-y-8 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Orders</h1>
		<p class="mt-1 text-sm text-muted">
			A request for assets, and somebody's authorisation to hand them over. Ordering is for assets
			you can see but not download yourself.
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
	<p role="status" aria-live="polite" class="text-sm text-muted">{notice}</p>

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else}
		{#if queue}
			<section aria-label="Waiting for a decision" class="space-y-2">
				<h2 class="text-xs font-semibold tracking-wide text-muted uppercase">
					Waiting for a decision{#if queue.length > 0}&nbsp;({queue.length}){/if}
				</h2>
				{#if queue.length === 0}
					<p class="text-sm text-muted">Nothing to decide.</p>
				{:else}
					<ul class="space-y-2">
						{#each queue as order (order.id)}
							<li class="rounded-md border border-line p-3 text-sm">
								<div class="flex flex-wrap items-baseline gap-x-2">
									<span class="font-mono text-xs">{order.reference}</span>
									<span class="font-medium">
										{order.requested_by?.name ?? 'Somebody'}
									</span>
									<span class="text-muted">
										· {order.items.length}
										{order.items.length === 1 ? 'asset' : 'assets'}
										{#if order.channel}· for {order.channel}{/if}
										{#if order.territory}in {order.territory}{/if}
									</span>
								</div>
								<!-- The reason, which is the entire question being answered. -->
								<p class="mt-1">{order.purpose}</p>
								<p class="mt-1 text-xs text-muted">
									{order.items.map((item) => item.filename).join(', ')}
								</p>
								{#if order.recipients.length > 0}
									<p class="mt-1 text-xs text-muted">For: {order.recipients.join(', ')}</p>
								{/if}

								{#if deciding === order.id}
									<div class="mt-2 space-y-2">
										<label class="block text-xs text-muted">
											A note, if it needs one
											<input
												bind:value={note}
												class="mt-0.5 w-full rounded-md border border-line bg-raised px-2 py-1 text-sm"
												placeholder="Print only."
											/>
										</label>
										<div class="flex gap-2">
											<button
												type="button"
												class="rounded-md bg-accent px-2 py-1 text-xs text-accent-fg"
												onclick={() => decide(order, 'approve')}
											>
												Approve
											</button>
											<button
												type="button"
												class="rounded-md border border-line px-2 py-1 text-xs"
												onclick={() => decide(order, 'reject')}
											>
												Refuse
											</button>
											<button
												type="button"
												class="text-xs underline"
												onclick={() => {
													deciding = null;
													note = '';
												}}
											>
												Cancel
											</button>
										</div>
									</div>
								{:else}
									<button
										type="button"
										class="mt-2 rounded-md border border-line px-2 py-1 text-xs"
										onclick={() => {
											deciding = order.id;
											note = '';
										}}
									>
										Decide…
									</button>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</section>
		{/if}

		<section aria-label="My orders" class="space-y-2">
			<h2 class="text-xs font-semibold tracking-wide text-muted uppercase">
				My orders{#if mine.length > 0}&nbsp;({mine.length}){/if}
			</h2>
			{#if mine.length === 0}
				<!--
					Said rather than blank, and it says where to start: an order is placed from a selection in the
					grid, which is not discoverable from an empty page.
				-->
				<p class="text-sm text-muted">
					You have not ordered anything. Select assets in
					<a class="underline" href={resolve('/assets')}>Assets</a> and choose Order.
				</p>
			{:else}
				<ul class="space-y-2">
					{#each mine as order (order.id)}
						<li class="rounded-md border border-line p-3 text-sm">
							<div class="flex flex-wrap items-baseline gap-x-2">
								<span class="font-mono text-xs">{order.reference}</span>
								<span class="rounded bg-raised px-1.5 py-0.5 text-xs font-medium">
									<!--
										The window closing is a different fact from the decision, so an expired pickup
										says so rather than still reading "ready".
									-->
									{order.expired && order.state === 'ready' ? 'expired' : order.state}
								</span>
								<span class="text-muted">
									· {order.items.length}
									{order.items.length === 1 ? 'asset' : 'assets'}
								</span>
								{#if order.expires_at && !order.expired}
									<span class="text-muted">· collect by {when(order.expires_at)}</span>
								{/if}
							</div>
							<p class="mt-1">{order.purpose}</p>
							{#if order.decided_by}
								<p class="mt-1 text-xs text-muted">
									{order.state === 'rejected' ? 'Refused' : 'Approved'} by {order.decided_by.name}
									{#if order.decision_note}— {order.decision_note}{/if}
								</p>
							{/if}
							{#if order.state === 'approved'}
								<!--
									Approved, and the pickup failed to be made. Not the ordinary path — approval makes
									the pickup in the same request — so this is the retryable case, and saying so beats
									a row that looks stuck for a reason nobody can see.
								-->
								<p class="mt-1 text-xs text-muted">
									Approved. The pickup could not be prepared; an administrator can retry it.
								</p>
							{/if}
							{#if (order.state === 'ready' || order.state === 'collected') && !order.expired}
								<!--
									Three things a requester needs once it is ready: that it is, the metadata if they
									asked for it, and the pickup link — which exists in readable form only in the
									response that minted it, so it is shown once and kept nowhere.
								-->
								<p class="mt-1 text-xs">
									Ready to collect.
									{#if order.include_metadata}
										<!--
											`rel="external"` because it is: the export lives on the API origin, not on a
											SvelteKit route, so the client router must hand it to the browser rather than
											try to resolve it. The lint that asked for this was right about the substance.
										-->
										<a class="underline" href={metadataHref(order)} rel="external" download>
											Download the metadata (CSV)
										</a>
									{/if}
								</p>
								{#if pickups[order.id]}
									<!--
										Shown once, because this is the only time it exists in readable form: a share
										token is stored as a digest. Held in memory and nowhere else — writing it to
										storage would turn a lost link into a leaked one — so it is gone on reload,
										which is what re-issuing is for.
									-->
									<p class="mt-1 text-xs break-all">
										Send this to {order.recipients.length > 0
											? order.recipients.join(', ')
											: 'the recipients'}:
										<code class="rounded bg-raised px-1 py-0.5">{pickups[order.id]}</code>
									</p>
								{:else}
									<p class="mt-1 text-xs text-muted">
										The pickup link was shown once when it was made. An administrator can issue a
										new one, which stops the old link working.
									</p>
								{/if}
							{/if}
							{#if queue && (order.state === 'ready' || order.state === 'collected')}
								<!--
									Offered only to somebody who can decide — the server refuses it otherwise, and a
									button that always 403s teaches people to ignore errors. Whether the queue loaded
									is how this page already knows.
								-->
								<button type="button" class="mt-2 text-xs underline" onclick={() => reissue(order)}>
									Issue a new pickup link
								</button>
							{/if}
							{#if order.state === 'submitted'}
								<button
									type="button"
									class="mt-2 text-xs underline"
									onclick={() => decide(order, 'cancel')}
								>
									Withdraw
								</button>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</div>
