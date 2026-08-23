<script lang="ts">
	/**
	 * The governance record, and the check that says it has not been edited (G10).
	 *
	 * ## The check is the feature, so it is the first thing on the page
	 *
	 * A hash chain nobody verifies is a column of hex. `audit_log` refuses UPDATE and DELETE by rule, which is
	 * a control an auditor can be shown and a control a superuser can drop — so the rule is the fence and the
	 * chain is the alarm. This screen is where somebody hears it.
	 *
	 * ## "We could not check" and "it has been altered" are different sentences
	 *
	 * The server answers a broken chain with a 200 and `intact: false`, deliberately, because a 500 would be
	 * indistinguishable from the database being down. The screen has to keep that distinction: a failed
	 * *request* is a neutral "could not check just now", and a failed *verification* is the alarming one, in
	 * the colour that means it.
	 *
	 * ## Exporting is an action with a consequence, and the consequence is shown
	 *
	 * The export appends an entry saying a copy was taken — so the list grows by one every time somebody
	 * presses the button, and a screen that did not say so would look like a bug. The panel names the entry it
	 * just created.
	 *
	 * The extract downloads as JSON rather than opening in a tab, because what it is for is being handed to
	 * somebody who will re-verify it elsewhere, and the hashes are the payload.
	 *
	 * ## Filters are the questions people actually ask
	 *
	 * Not a query builder. "What happened to this asset", "what has this person done", "every hold ever
	 * placed" — three questions, and the third is a preset because "legal_hold.placed" is not a string anybody
	 * should have to know.
	 */
	import { onMount } from 'svelte';
	import {
		ApiError,
		auditLog,
		exportAudit,
		verifyAudit,
		type AuditEntry,
		type AuditVerification
	} from '$lib/api/client';

	/** The actions worth offering as a filter, in the words somebody would use. */
	const PRESETS = [
		{ value: '', label: 'Everything' },
		{ value: 'legal_hold.placed', label: 'Holds placed' },
		{ value: 'legal_hold.lifted', label: 'Holds released' },
		{ value: 'connector.registered', label: 'Sites connected' },
		{ value: 'connector.rotated', label: 'Site secrets rotated' },
		{ value: 'connector.revoked', label: 'Sites revoked' },
		{ value: 'audit.exported', label: 'Record exported' }
	];

	/** How each action reads in a sentence. An unknown action is shown as itself, never guessed at. */
	const PHRASING: Record<string, string> = {
		'legal_hold.placed': 'placed a legal hold',
		'legal_hold.lifted': 'released a legal hold',
		'retention.changed': 'changed a retention policy',
		'erasure.requested': 'requested an erasure',
		'erasure.completed': 'completed an erasure',
		'erasure.refused': 'refused an erasure',
		'connector.registered': 'connected a site',
		'connector.rotated': 'rotated a site secret',
		'connector.revoked': 'revoked a site',
		'identity.provisioned': 'provisioned an account',
		'identity.deprovisioned': 'deprovisioned an account',
		'identity.reactivated': 'reactivated an account',
		'role.granted': 'granted a role',
		'role.revoked': 'revoked a role',
		'key.issued': 'issued an API key',
		'key.revoked': 'revoked an API key',
		'audit.exported': 'exported the record',
		'support.access': 'accessed this tenant as support'
	};

	let entries = $state<AuditEntry[]>([]);
	let nextBeforeSeq = $state<number | null>(null);
	let loading = $state(true);
	let error = $state('');
	let forbidden = $state(false);

	let action = $state('');
	let targetId = $state('');

	let verification = $state<AuditVerification | null>(null);
	let verifying = $state(false);
	/** A failed *request*, as opposed to a failed verification. Different sentences, different colours. */
	let verifyError = $state('');

	let exporting = $state(false);
	let exportNotice = $state('');

	async function load(before?: number) {
		loading = true;
		error = '';
		try {
			const page = await auditLog({
				action: action || undefined,
				target_kind: targetId ? 'asset' : undefined,
				target_id: targetId || undefined,
				before_seq: before
			});
			entries = before === undefined ? page.entries : [...entries, ...page.entries];
			nextBeforeSeq = page.next_before_seq ?? null;
		} catch (cause) {
			if (cause instanceof ApiError && cause.status === 403) forbidden = true;
			else error = cause instanceof Error ? cause.message : 'Could not read the record.';
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void load();
		void check();
	});

	async function check() {
		verifying = true;
		verifyError = '';
		try {
			verification = await verifyAudit();
		} catch (cause) {
			// Not the same thing as a broken chain, and it must not be shown as one.
			if (cause instanceof ApiError && cause.status === 403) forbidden = true;
			else verifyError = cause instanceof Error ? cause.message : 'Could not run the check.';
		} finally {
			verifying = false;
		}
	}

	async function download() {
		exporting = true;
		exportNotice = '';
		try {
			const extract = await exportAudit(0);
			const blob = new Blob([JSON.stringify(extract, null, 2)], { type: 'application/json' });
			const url = URL.createObjectURL(blob);
			const link = document.createElement('a');
			link.href = url;
			link.download = 'audit-extract.json';
			link.click();
			URL.revokeObjectURL(url);
			exportNotice = `${extract.entries.length} entries. Recorded as entry ${extract.recorded_as.seq} — taking a copy is itself in the record.`;
			// The list is now one entry short of the truth, and that entry is the one just described.
			await load();
		} catch (cause) {
			exportNotice = cause instanceof Error ? cause.message : 'Could not export.';
		} finally {
			exporting = false;
		}
	}

	function phrasing(entry: AuditEntry): string {
		return PHRASING[entry.action] ?? entry.action;
	}

	function actorOf(entry: AuditEntry): string {
		if (entry.actor_kind === 'system') return 'The system';
		if (entry.actor_kind === 'support') return 'Support';
		if (entry.actor_kind === 'connector') return 'A connected site';
		// No display name is carried: the log stores an identity id, and resolving it against the current
		// directory would show today's name for a person who may since have left. The id is the fact.
		if (entry.actor_id) return `Identity ${entry.actor_id.slice(0, 8)}`;
		return 'An API key';
	}

	function reasonOf(entry: AuditEntry): string {
		const payload = entry.payload as Record<string, unknown> | null | undefined;
		const parts: string[] = [];
		for (const key of ['reason', 'label', 'filename']) {
			const value = payload?.[key];
			if (typeof value === 'string' && value !== '') parts.push(value);
		}
		return parts.join(' · ');
	}
</script>

<svelte:head>
	<title>Governance · damrs</title>
</svelte:head>

<div class="mx-auto max-w-4xl p-6">
	<h1 class="text-xl font-semibold">Governance record</h1>
	<p class="mt-1 text-sm text-muted">
		Every hold, every connected site, every export — hash-chained, so an alteration shows up as one.
	</p>

	{#if forbidden}
		<p class="mt-4 rounded-md bg-surface p-3 text-sm" role="status">
			This record needs administrator access. Ask whoever manages this library.
		</p>
	{:else}
		<section
			class="mt-4 rounded-md bg-surface p-3"
			aria-labelledby="check-heading"
			data-testid="integrity"
		>
			<div class="flex items-center justify-between gap-3">
				<h2 id="check-heading" class="text-sm font-semibold">Integrity</h2>
				<div class="flex gap-2">
					<button
						type="button"
						onclick={check}
						disabled={verifying}
						class="rounded border border-line px-3 py-1 text-sm disabled:opacity-50"
					>
						{verifying ? 'Checking…' : 'Check again'}
					</button>
					<button
						type="button"
						onclick={download}
						disabled={exporting}
						class="rounded bg-accent px-3 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
					>
						{exporting ? 'Exporting…' : 'Export'}
					</button>
				</div>
			</div>

			{#if verifyError}
				<p class="mt-2 text-sm text-muted" role="status">
					Could not run the check just now — {verifyError}
				</p>
			{:else if verification === null}
				<p class="mt-2 text-sm text-muted">Checking…</p>
			{:else if verification.intact}
				<p class="mt-2 text-sm" role="status">
					Intact. {verification.checked.toLocaleString()}
					{verification.checked === 1 ? 'entry' : 'entries'} checked, and each one hashes to the value
					stored beside it.
				</p>
			{:else}
				<p class="mt-2 text-sm font-medium text-state-rights-denied-fg" role="alert">
					{#if verification.failure?.kind === 'unlinked'}
						An entry is missing. Entry {verification.failure.seq} names a predecessor that is not the
						entry before it.
					{:else}
						Entry {verification.failure?.seq} has been altered since it was written.
					{/if}
				</p>
				<p class="mt-1 text-xs text-muted">{verification.failure?.detail}</p>
				<p class="mt-1 text-xs text-muted">
					The database refuses edits and deletions by rule, so this needed database-level access.
					Everything from this entry onward should be treated as unverified.
				</p>
			{/if}

			{#if exportNotice}
				<p class="mt-2 text-xs text-muted" role="status">{exportNotice}</p>
			{/if}
		</section>

		<div class="mt-4 flex flex-wrap items-end gap-3">
			<label class="text-xs font-medium">
				Show
				<select
					bind:value={action}
					onchange={() => load()}
					class="mt-1 block rounded border border-line bg-bg px-2 py-1 text-sm"
				>
					{#each PRESETS as preset (preset.value)}
						<option value={preset.value}>{preset.label}</option>
					{/each}
				</select>
			</label>
			<label class="text-xs font-medium">
				For asset
				<input
					type="text"
					bind:value={targetId}
					onchange={() => load()}
					placeholder="asset id"
					class="mt-1 block w-72 rounded border border-line bg-bg px-2 py-1 text-sm"
				/>
			</label>
		</div>

		{#if error}
			<p class="mt-4 text-sm text-state-rights-denied-fg" role="alert">{error}</p>
		{/if}

		{#if loading && entries.length === 0}
			<p class="mt-4 text-sm text-muted">Reading the record…</p>
		{:else if entries.length === 0}
			<p class="mt-4 text-sm text-muted">
				Nothing recorded yet. Governance actions — a legal hold, a connected site — land here as
				they happen.
			</p>
		{:else}
			<ol class="mt-4 space-y-2">
				{#each entries as entry (entry.seq)}
					<li class="rounded-md bg-surface p-3" data-testid="entry-{entry.seq}">
						<div class="flex items-baseline justify-between gap-3">
							<p class="text-sm">
								<span class="font-medium">{actorOf(entry)}</span>
								{phrasing(entry)}
							</p>
							<span class="shrink-0 text-xs text-muted">
								{new Date(entry.at).toLocaleString()}
							</span>
						</div>
						{#if reasonOf(entry)}
							<p class="mt-1 text-xs text-muted">{reasonOf(entry)}</p>
						{/if}
						<p class="mt-1 font-mono text-[11px] text-muted">
							entry {entry.seq} · {entry.hash.slice(0, 16)}…
						</p>
					</li>
				{/each}
			</ol>

			{#if nextBeforeSeq !== null}
				<button
					type="button"
					onclick={() => load(nextBeforeSeq ?? undefined)}
					disabled={loading}
					class="mt-3 rounded border border-line px-3 py-1 text-sm disabled:opacity-50"
				>
					{loading ? 'Loading…' : 'Older entries'}
				</button>
			{/if}
		{/if}
	{/if}
</div>
