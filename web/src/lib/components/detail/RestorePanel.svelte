<!--
	Asking for an archived original back (§6.5, F.11b·2b).

	## The price comes before the button

	§6.5's requirement is the estimate *before* the confirmation, and the reason is the spread: Expedited
	against Bulk is roughly 10× on cost and 100× on latency, and somebody picking without seeing either is
	guessing with their employer's money. So this panel opens by quoting all three tiers and shows the choice
	as a comparison, not as a menu of words.

	## A tier the class cannot offer stays on screen, refused

	Deep Archive has no Expedited. Hiding the row would give one asset two choices and another three, which
	invites "why is this one different" — a question the quote already answers, so it is shown.

	## While it runs, the panel is a status, not a form

	Once a restore is in flight the tiers are gone and what is left is the ETA and what it is doing. Offering
	the buttons again would offer a second retrieval — the server coalesces, so pressing them is harmless, but
	a button that does nothing is worse than no button.
-->
<script lang="ts">
	import {
		currentRestore,
		requestRestore,
		restoreQuote,
		type Restore,
		type RestoreQuoteOption
	} from '$lib/api/client';

	let { assetId, tier }: { assetId: string; tier: string } = $props();

	let options = $state<RestoreQuoteOption[]>([]);
	let current = $state<Restore | null>(null);
	let chosen = $state('standard');
	let error = $state('');
	let asking = $state(false);

	/** The states where something is happening and the tiers should not be offered. */
	const IN_FLIGHT = ['queued', 'awaiting_approval', 'requested', 'ongoing'];

	const running = $derived(current !== null && IN_FLIGHT.includes(current.state));

	/** What each state means, in the words somebody waiting needs. */
	const EXPLANATION: Record<string, string> = {
		queued: 'Queued. The next worker pass will ask the storage provider for a copy.',
		awaiting_approval:
			'Held for approval — this is over the spend threshold, so an administrator has to release it.',
		requested: 'Asked for. The provider is working on it.',
		ongoing: 'The provider is working on it.',
		available: 'A copy is ready. The original can be downloaded until the copy expires.',
		expired: 'The temporary copy has lapsed. Asking again will fetch another.',
		failed: 'The last attempt failed. Asking again is safe.',
		cancelled: 'Cancelled.'
	};

	async function load() {
		error = '';
		try {
			// Both, always. The quote is what the form needs and the current request is what decides whether
			// there is a form at all — and fetching them in sequence would flash the tiers on screen for
			// anybody whose restore is already running.
			const [quoted, existing] = await Promise.all([
				restoreQuote(assetId).catch(() => ({ options: [] })),
				currentRestore(assetId).catch(() => null)
			]);
			options = quoted.options;
			current = existing;
			const first = options.find((option) => option.available);
			if (first && !options.some((option) => option.tier === chosen && option.available)) {
				chosen = first.tier;
			}
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not read the restore options.';
		}
	}

	// Keyed on the asset, so moving between assets in the grid re-reads rather than showing the previous
	// one's quote under a new filename.
	$effect(() => {
		if (tier === 'archive' || tier === 'restoring' || tier === 'restored') {
			void load();
		}
	});

	async function ask() {
		asking = true;
		error = '';
		try {
			current = await requestRestore(assetId, chosen);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not ask for a restore.';
		} finally {
			asking = false;
		}
	}

	/** Whole cents as money. Zero means the pool has no prices recorded, which is not the same as free. */
	function money(cents: number): string {
		if (cents === 0) return 'not priced';
		if (cents < 100) return `${cents}¢`;
		return `$${(cents / 100).toFixed(2)}`;
	}

	function eta(iso: string | null | undefined): string {
		if (!iso) return 'unknown';
		const at = new Date(iso);
		const minutes = Math.round((at.getTime() - Date.now()) / 60000);
		if (minutes < 90) return `~${Math.max(1, minutes)} min`;
		const hours = Math.round(minutes / 60);
		if (hours < 36) return `~${hours} h`;
		return `~${Math.round(hours / 24)} days`;
	}
</script>

{#if tier === 'archive' || tier === 'restoring'}
	<section
		class="space-y-2 rounded-md border border-line p-3"
		aria-label="Restore from cold storage"
	>
		{#if error}
			<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
		{/if}

		{#if running && current}
			<p class="text-xs font-semibold tracking-tight">Restoring</p>
			<p class="text-xs text-muted">{EXPLANATION[current.state] ?? current.state}</p>
			{#if current.eta_at}
				<p class="text-xs text-muted">
					Expected <time datetime={current.eta_at}>{eta(current.eta_at)}</time> from now.
				</p>
			{/if}
			{#if current.joined_existing}
				<p class="text-xs text-muted">
					Somebody had already asked for this one, so you are waiting on the same copy.
				</p>
			{/if}
		{:else}
			<p class="text-xs font-semibold tracking-tight">Restore the original</p>
			<p class="text-xs text-muted">
				The bytes are in cold storage. Choose how fast, and what that costs — the estimate is the
				provider's retrieval charge for this file.
			</p>

			<fieldset class="space-y-1">
				<legend class="sr-only">Retrieval speed</legend>
				{#each options as option (option.tier)}
					<label
						class="flex items-center gap-2 rounded px-1 py-1 text-xs {option.available
							? ''
							: 'opacity-50'}"
					>
						<input
							type="radio"
							name="restore-tier"
							value={option.tier}
							disabled={!option.available}
							checked={chosen === option.tier}
							onchange={() => (chosen = option.tier)}
						/>
						<span class="w-20 capitalize">{option.tier}</span>
						{#if option.available}
							<span class="tabular-nums">{eta(option.eta_at)}</span>
							<span class="text-muted tabular-nums">{money(option.est_cost_cents)}</span>
							{#if option.needs_approval}
								<span class="text-muted">needs approval</span>
							{/if}
						{:else}
							<span class="text-muted">{option.unavailable_because}</span>
						{/if}
					</label>
				{/each}
			</fieldset>

			<button
				type="button"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
				disabled={asking || options.every((option) => !option.available)}
				onclick={ask}
			>
				{asking ? 'Asking…' : 'Restore'}
			</button>
		{/if}
	</section>
{:else if tier === 'restored' && current?.expires_at}
	<!-- A restored copy is temporary and the storage class never changed, so the expiry is the thing to say:
	     after it, the same asset needs restoring again. -->
	<p class="text-xs text-muted">
		A restored copy is available until <time datetime={current.expires_at}
			>{new Date(current.expires_at).toLocaleString()}</time
		>.
	</p>
{/if}
