<!--
	Freezing an asset, and the record of who froze it (G10).

	## The badge existed before the button did

	`assets.legal_hold` has been read since the first migration — the rights gate refuses to deliver a held
	asset, the tiering scan refuses to move one, the detail panel draws a badge for it — and until now nothing
	could set it. This is the control, and it is here rather than on an administration screen because the
	decision is about *this* asset and is taken while looking at it.

	## The reason is the point of the interaction

	There is no `legal_hold_reason` column and this panel does not want one. The reason goes into the audit
	entry, along with who and when, and the three together are what somebody needs two years later. A field
	that overwrote itself on every change would answer the least useful third of the question.

	So the button is disabled until a reason is typed. Not submitted-then-rejected: a form that lets you press
	the consequential button and then says no has taught you nothing about why.

	## Lifting is the direction that gets the confirmation

	Placing a hold is conservative — it blocks deletion and tiering, and the cost of a wrong one is an asset
	that stays put. Lifting removes a legal protection, and it is the one an auditor reads first. So the copy
	for lifting is explicit about what stops applying.

	## The history is read back from the log, not from the asset

	Which is what makes this a governance surface rather than a toggle: the panel shows every previous hold and
	release on this asset with its reason and its author, because "is it held" is rarely the whole question.
-->
<script lang="ts">
	import { ApiError, auditLog, setLegalHold, type AuditEntry } from '$lib/api/client';

	let {
		assetId,
		held,
		onchange
	}: { assetId: string; held: boolean; onchange?: (held: boolean) => void } = $props();

	let reason = $state('');
	let busy = $state(false);
	let error = $state('');
	let notice = $state('');
	let history = $state<AuditEntry[]>([]);
	let loadingHistory = $state(true);
	/** False when this caller may not read the log — a 403 here is not an error worth shouting about. */
	let mayReadLog = $state(true);
	/**
	 * Why the previous holds could not be listed, if they could not.
	 *
	 * Separate from `error`, and not an `alert`. `error` is for a failed *write* — something the person just
	 * attempted and can retry — while this is a read they did not ask for, of a record they cannot repair. A
	 * red alert about the audit log, next to a button that works, is noise attached to the wrong thing.
	 */
	let historyProblem = $state('');

	const HOLD_ACTIONS = ['legal_hold.placed', 'legal_hold.lifted'];

	async function loadHistory() {
		loadingHistory = true;
		historyProblem = '';
		try {
			const page = await auditLog({ target_kind: 'asset', target_id: assetId });
			history = page.entries.filter((entry) => HOLD_ACTIONS.includes(entry.action));
		} catch (cause) {
			// A curator with manage on the asset may still not hold the gate the audit log takes. The control
			// stays usable; only the history goes away, with no error to explain something they cannot fix.
			if (cause instanceof ApiError && cause.status === 403) mayReadLog = false;
			else historyProblem = cause instanceof Error ? cause.message : 'Could not read the record.';
		} finally {
			loadingHistory = false;
		}
	}

	$effect(() => {
		void assetId;
		void loadHistory();
	});

	async function apply(next: boolean) {
		if (reason.trim() === '') return;
		busy = true;
		error = '';
		notice = '';
		try {
			const result = await setLegalHold(assetId, next, reason);
			// `changed: false` means it was already in this state and nothing was recorded. Saying "placed"
			// would claim an entry that does not exist.
			notice = result.changed
				? next
					? `Held. Recorded as entry ${result.audit_seq}.`
					: `Released. Recorded as entry ${result.audit_seq}.`
				: 'Already in that state — nothing recorded.';
			reason = '';
			onchange?.(result.held);
			await loadHistory();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not change the hold.';
		} finally {
			busy = false;
		}
	}

	function when(at: string): string {
		return new Date(at).toLocaleString();
	}

	// The payload is `jsonb` on the server, so the generated type is an open object. Read through one narrow
	// accessor rather than casting at every use: the shape is the server's to change, and a screen that
	// assumed it would break silently on a field rename.
	function reasonOf(entry: AuditEntry): string {
		const payload = entry.payload as Record<string, unknown> | null | undefined;
		const value = payload?.reason;
		return typeof value === 'string' ? value : '';
	}
</script>

<section class="rounded-md bg-surface p-3" aria-labelledby="legal-hold-heading">
	<h3 id="legal-hold-heading" class="text-sm font-semibold">Legal hold</h3>

	<p class="mt-1 text-xs text-muted">
		{#if held}
			This asset is held. It cannot be deleted, and storage tiering will leave it where it is.
		{:else}
			Not held. A hold blocks deletion and stops storage tiering moving the asset.
		{/if}
	</p>

	<label class="mt-3 block text-xs font-medium" for="legal-hold-reason">
		Reason {#if !held}(required){:else}for releasing (required){/if}
	</label>
	<input
		id="legal-hold-reason"
		type="text"
		bind:value={reason}
		disabled={busy}
		placeholder={held ? 'e.g. matter 2026-114 closed' : 'e.g. litigation hold, matter 2026-114'}
		class="mt-1 w-full rounded border border-line bg-bg px-2 py-1 text-sm"
	/>
	<p class="mt-1 text-xs text-muted">
		Kept in the governance record with your name and the time. It is not stored on the asset.
	</p>

	<button
		type="button"
		onclick={() => apply(!held)}
		disabled={busy || reason.trim() === ''}
		class="mt-2 rounded bg-accent px-3 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
	>
		{#if busy}
			Working…
		{:else if held}
			Release the hold
		{:else}
			Place a hold
		{/if}
	</button>

	{#if notice}
		<p class="mt-2 text-xs" role="status">{notice}</p>
	{/if}
	{#if error}
		<p class="mt-2 text-xs text-state-rights-denied-fg" role="alert">{error}</p>
	{/if}

	{#if mayReadLog}
		<!-- "Previous holds", not "History": the asset's own activity feed is a panel in this same sidebar
		     under that word, and two headings called History leave a reader guessing which one they are
		     reading. Naming what it lists is shorter as well as clearer. -->
		<h4 class="mt-4 text-xs font-semibold">Previous holds</h4>
		{#if loadingHistory}
			<p class="mt-1 text-xs text-muted">Reading the record…</p>
		{:else if historyProblem}
			<!-- Said once, and not as "no holds": a read that failed is in no position to claim there are none. -->
			<p class="mt-1 text-xs text-muted">Could not read the record — {historyProblem}</p>
		{:else if history.length === 0}
			<p class="mt-1 text-xs text-muted">No hold has ever been placed on this asset.</p>
		{:else}
			<ul class="mt-1 space-y-1.5">
				{#each history as entry (entry.seq)}
					<li class="text-xs">
						<span class="font-medium">
							{entry.action === 'legal_hold.placed' ? 'Held' : 'Released'}
						</span>
						<span class="text-muted"> · {when(entry.at)} · entry {entry.seq}</span>
						{#if reasonOf(entry)}
							<div class="text-muted">{reasonOf(entry)}</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</section>
