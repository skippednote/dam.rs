<script lang="ts">
	/**
	 * Share management: every link the tenant has issued, and the revoke that makes them stop.
	 *
	 * The list is what makes revocation *findable* — a share you cannot see is a share you cannot revoke, and
	 * revocation is the whole safety story for a link that escaped to the wrong inbox. Tokens are digests
	 * server-side and never reappear here: a lost link is revoked and re-created, like an API key.
	 */
	import { onMount } from 'svelte';
	import { ApiError, listShares, revokeShare, type ShareRow } from '$lib/api/client';

	let rows = $state<ShareRow[]>([]);
	let error = $state('');
	let busy = $state<string | null>(null);

	async function load() {
		error = '';
		try {
			rows = await listShares();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not load shares.';
		}
	}

	async function revoke(id: string) {
		busy = id;
		try {
			await revokeShare(id);
			await load();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not revoke.';
		} finally {
			busy = null;
		}
	}

	function when(iso: string): string {
		return new Date(iso).toLocaleDateString();
	}

	onMount(load);
</script>

<div class="mx-auto max-w-4xl space-y-4 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Shares</h1>
		<p class="mt-1 text-sm text-muted">
			Links are created from an asset's detail panel. Revoking one takes effect immediately —
			including on downloads it already handed out.
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

	{#if rows.length === 0 && !error}
		<p class="text-sm text-muted">No shares yet.</p>
	{:else if rows.length > 0}
		<!-- A real table: this is tabular data, and a table is how a screen reader navigates it by column. -->
		<div class="overflow-x-auto rounded-md border border-line">
			<table class="w-full text-sm">
				<thead>
					<tr class="border-b border-line text-left text-xs tracking-wide text-muted uppercase">
						<th class="px-3 py-2 font-semibold">Asset</th>
						<th class="px-3 py-2 font-semibold">Created</th>
						<th class="px-3 py-2 font-semibold">Expires</th>
						<th class="px-3 py-2 font-semibold">Downloads</th>
						<th class="px-3 py-2 font-semibold">Protection</th>
						<th class="px-3 py-2 font-semibold">State</th>
						<th class="px-3 py-2"><span class="sr-only">Actions</span></th>
					</tr>
				</thead>
				<tbody>
					{#each rows as row (row.id)}
						<!-- Dead rows recede by *ground*, not opacity: opacity multiplies into the text color and
							     drops small muted text below AA contrast. -->
						<tr class="border-b border-line last:border-b-0 {row.live ? '' : 'bg-surface'}">
							<td class="max-w-48 truncate px-3 py-2" title={row.filename ?? undefined}>
								{row.filename ?? '(deleted asset)'}
							</td>
							<td class="px-3 py-2 whitespace-nowrap">{when(row.created_at)}</td>
							<td class="px-3 py-2 whitespace-nowrap">
								{row.expires_at ? when(row.expires_at) : 'never'}
							</td>
							<td class="px-3 py-2 whitespace-nowrap">
								{row.download_count}{row.max_downloads ? ` / ${row.max_downloads}` : ''}
							</td>
							<td class="px-3 py-2 text-xs text-muted">
								{[
									row.has_passcode ? 'passcode' : null,
									row.allow_original ? 'original' : 'web only'
								]
									.filter(Boolean)
									.join(' · ')}
							</td>
							<td class="px-3 py-2">
								{#if row.revoked}
									<span class="text-xs text-muted">revoked</span>
								{:else if row.live}
									<span class="text-xs font-medium text-state-rights-allowed-fg">live</span>
								{:else}
									<span class="text-xs text-muted">expired</span>
								{/if}
							</td>
							<td class="px-3 py-2 text-right">
								{#if row.live}
									<button
										type="button"
										class="text-xs text-state-rights-denied-fg underline disabled:opacity-50"
										disabled={busy === row.id}
										onclick={() => revoke(row.id)}
									>
										Revoke
									</button>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</div>
