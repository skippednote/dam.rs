<script lang="ts">
	/**
	 * The tenant's caps, and how close they are.
	 *
	 * ## This screen exists so a cap is never a surprise
	 *
	 * A hard cap refuses an upload with a 507 and no body — correct for the protocol and useless as an
	 * explanation. So the explanation lives here, ahead of time: what the limit is, where the warning line sits,
	 * and whether crossing it will stop work or merely be noted.
	 *
	 * ## A level and a flow are the same number meaning different things
	 *
	 * "900 of 1,000" is *what exists* for storage and *what happened this month* for spend. Drawing one bar for
	 * both would be misleading about the more alarming one — a month of egress read as the size of a library is
	 * a very different conversation. So every row says which, in words.
	 *
	 * ## A soft cap over its limit is not an error
	 *
	 * It is the deliberate default: a hard cap on ingest loses a customer's work, so enforcement is per quota
	 * and `soft` is what an operator gets unless they ask otherwise. A row that is over a soft cap says the work
	 * continues, because otherwise it reads as an outage.
	 *
	 * ## Absent caps are absent
	 *
	 * The server sends only configured ones. Listing every possible key at zero would read as a tenant who had
	 * exhausted everything, and would contradict the enforcement — which allows work against a key with no row.
	 *
	 * ## Nothing here can change a limit, and the page says so
	 *
	 * A tenant raising its own cap is not a feature. Leaving the fields read-only without explaining would just
	 * look like a missing button.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { ApiError, loadQuotas, type Quota, type Quotas } from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let data = $state<Quotas | null>(null);
	let loading = $state(true);
	let error = $state('');

	onMount(async () => {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			data = await loadQuotas();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Caps are administration; this key does not hold Manage.'
					: caught instanceof ApiError
						? caught.message
						: 'Could not read the caps.';
		} finally {
			loading = false;
		}
	});

	/** What each key counts, in words. The key itself is machine vocabulary. */
	const LABELS: Record<string, string> = {
		storage_bytes: 'Storage',
		asset_count: 'Assets',
		egress_bytes_month: 'Downloads this month',
		ai_spend_cents_month: 'AI spend this month',
		restore_spend_cents_month: 'Retrieval spend this month',
		api_requests_minute: 'API requests a minute',
		seats: 'People'
	};

	/** How to render the number — the unit is part of the meaning, not decoration. */
	function amount(key: string, value: number): string {
		if (key.endsWith('_bytes') || key.endsWith('bytes_month')) return bytes(value);
		if (key.endsWith('cents_month')) return money(value);
		return value.toLocaleString();
	}

	function bytes(value: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
		let n = value;
		let unit = 0;
		while (n >= 1024 && unit < units.length - 1) {
			n /= 1024;
			unit += 1;
		}
		return `${n < 10 && unit > 0 ? n.toFixed(1) : Math.round(n)} ${units[unit]}`;
	}

	/** Cents, shown as currency-neutral units: damrs does not know the tenant's currency. */
	function money(cents: number): string {
		return `${(cents / 100).toFixed(2)}`;
	}

	function fraction(row: Quota): number {
		return row.limit_value > 0 ? Math.min(1, Math.max(0, row.used / row.limit_value)) : 0;
	}

	function stamp(at: string | null | undefined): string {
		return at ? new Date(at).toLocaleString() : '';
	}
</script>

<svelte:head><title>Limits · damrs</title></svelte:head>

<div class="mx-auto max-w-2xl space-y-6 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Limits</h1>
		<p class="mt-1 max-w-prose text-sm text-muted">
			What this account is capped at, and how close it is. Nothing here can be changed from the
			application — a cap is part of the agreement rather than a setting — so raising one is a
			request to whoever runs this library.
		</p>
	</div>

	<nav aria-label="Settings" class="flex gap-3 text-sm">
		<a class="rounded-md px-2.5 py-1 text-muted hover:text-fg" href={resolve('/settings')}>
			Connection
		</a>
		<a class="rounded-md px-2.5 py-1 text-muted hover:text-fg" href={resolve('/settings/ai')}>
			AI models
		</a>
		<span aria-current="page" class="rounded-md bg-surface px-2.5 py-1 font-medium">Limits</span>
	</nav>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key on <a class="underline" href={resolve('/settings')}
				>Connection</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if !data || data.quotas.length === 0}
		<p class="max-w-prose text-sm text-muted" data-testid="no-caps">
			No caps are set on this account. Nothing here is limited, and an unset cap is not a cap of
			zero — uploads, downloads and enrichment all proceed.
		</p>
	{:else}
		<p class="text-xs text-muted">
			Monthly figures cover {new Date(data.period_start).toLocaleDateString(undefined, {
				month: 'long',
				year: 'numeric'
			})} — a calendar month, so they line up with the bill they protect.
		</p>

		<ul class="space-y-4" data-testid="caps">
			{#each data.quotas as row (row.quota_key)}
				<li
					class="space-y-1.5 rounded-md border p-3 {row.standing === 'refused'
						? 'border-state-rights-denied'
						: row.standing === 'warned'
							? 'border-state-rights-expiring'
							: 'border-line'}"
					data-testid="cap-{row.quota_key}"
				>
					<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
						<h2 class="text-sm font-semibold tracking-tight">
							{LABELS[row.quota_key] ?? row.quota_key}
						</h2>
						<span class="tabular-nums">
							{amount(row.quota_key, row.used)} of {amount(row.quota_key, row.limit_value)}
						</span>
						<!--
							Said in words, because the same figure means different things. A bar labelled only
							with a percentage would let a month of downloads read as the size of the library.
						-->
						<span class="text-xs text-muted">
							{row.is_level ? 'held right now' : 'used this month'}
						</span>
						{#if row.standing === 'refused'}
							<span
								class="rounded border border-state-rights-denied px-1.5 py-0.5 text-xs text-state-rights-denied-fg"
							>
								{row.enforcement === 'hard' ? 'new work refused' : 'over'}
							</span>
						{:else if row.standing === 'warned'}
							<span
								class="rounded border border-state-rights-expiring px-1.5 py-0.5 text-xs text-state-rights-expiring-fg"
							>
								close to the limit
							</span>
						{/if}
					</div>

					<!--
						`aria-hidden`, with the numbers above it as the accessible version. A progress bar
						announced as "90 percent" adds nothing to "900 of 1,000 held right now", and the warning
						line inside it has no announceable meaning at all.
					-->
					<div aria-hidden="true" class="relative h-1.5 overflow-hidden rounded-full bg-surface">
						<div
							class="h-full rounded-full {row.standing === 'refused'
								? 'bg-state-rights-denied'
								: row.standing === 'warned'
									? 'bg-state-rights-expiring'
									: 'bg-accent'}"
							style="width: {fraction(row) * 100}%"
						></div>
						<!-- The configured line, not an assumed 80%: an operator may have set it anywhere. -->
						<div
							class="absolute top-0 h-full w-px bg-fg/40"
							style="left: {Math.min(100, row.warn_at_fraction * 100)}%"
						></div>
					</div>

					<p class="text-xs text-muted">
						{#if row.enforcement === 'hard'}
							At the limit, new work is refused.
						{:else}
							<!-- The deliberate default, and it must not read as an outage. -->
							Over the limit, work continues and this is noted — nothing stops.
						{/if}
						<!--
							Past tense when it is past. The stamps never clear — that is deliberate, so "we were
							not told" stays answerable — but a row currently only *warned* showing "Over since"
							reads as over right now. Found by looking at a real tenant whose cap had been raised:
							183 of 200, "close to the limit", and "Over since 3:34am" underneath it.
						-->
						{#if row.exceeded_at && row.standing === 'refused'}
							<span data-testid="exceeded-{row.quota_key}">
								Over since {stamp(row.exceeded_at)}.
							</span>
						{:else if row.exceeded_at}
							<span data-testid="was-over-{row.quota_key}">
								Was over on {stamp(row.exceeded_at)}, and is not now.
							</span>
						{:else if row.warned_at}
							<span data-testid="warned-{row.quota_key}">
								Past the warning line since {stamp(row.warned_at)}.
							</span>
						{/if}
					</p>
				</li>
			{/each}
		</ul>
	{/if}
</div>
