<!--
	The version history of one asset.

	## Hidden when there is only one

	Every asset is version 1 of itself, so a panel headed "Versions (1)" on every asset in the library is noise on
	the overwhelming majority of them. It appears when there is a history to read, and the way to *start* one lives
	with the upload flow rather than here — see the note on the empty state.

	## "Current" is a state, not a button on the current one

	The row that is current says so and offers nothing; the others offer "Make current". A disabled control on the
	current row would be a control that exists to be unavailable.

	## Making an earlier version current keeps its number

	A promotion, not a copy. Renumbering version 2 as version 4 would claim somebody uploaded something they did
	not, and the history is the one place that has to stay literal.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { ApiError, listVersions, makeVersionCurrent, type AssetVersion } from '$lib/api/client';

	let {
		assetId,
		/** So the page can reload the grid when the current version changes underneath it. */
		onchanged
	}: {
		assetId: string;
		onchanged?: () => void;
	} = $props();

	let versions = $state<AssetVersion[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let loaded = $state(false);

	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			versions = [];
			error = '';
			notice = '';
			loaded = false;
			void load();
		});
	});

	async function load() {
		try {
			versions = await listVersions(assetId);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the version history.';
		} finally {
			loaded = true;
		}
	}

	function promote(version: AssetVersion) {
		busy = true;
		error = '';
		void (async () => {
			try {
				versions = await makeVersionCurrent(version.asset_id);
				notice = `Version ${version.version_no} is current again. Its number is unchanged — this is a promotion, not a new upload.`;
				onchanged?.();
			} catch (caught) {
				error = caught instanceof ApiError ? caught.message : 'That could not be changed.';
			} finally {
				busy = false;
			}
		})();
	}

	function bytes(n: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}

	function when(iso: string): string {
		return new Date(iso).toLocaleDateString();
	}
</script>

<!-- Nothing at all for a single-version asset: see the component docs. -->
{#if loaded && versions.length > 1}
	<section class="space-y-2" aria-label="Version history">
		<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">
			Versions ({versions.length})
		</h3>

		{#if error}
			<p
				role="alert"
				class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
			>
				{error}
			</p>
		{/if}
		<p role="status" aria-live="polite" class="sr-only">{notice}</p>

		<ol class="space-y-1">
			{#each versions as version (version.asset_id)}
				<li
					class="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md border border-line px-2 py-1.5 text-xs"
					class:bg-surface={!version.is_current}
				>
					<span class="font-medium tabular-nums">v{version.version_no}</span>
					{#if version.is_current}
						<!-- A state, not a control. See the component docs. -->
						<span
							class="rounded bg-state-rights-allowed/18 px-1.5 py-0.5 font-medium text-state-rights-allowed-fg"
						>
							current
						</span>
					{/if}
					<span class="min-w-0 flex-1 truncate">{version.filename}</span>
					<span class="text-muted tabular-nums">{bytes(version.bytes)}</span>
					<time datetime={version.created_at} class="text-muted">{when(version.created_at)}</time>
					{#if version.uploaded_by}
						<span class="text-muted">{version.uploaded_by.name}</span>
					{/if}
					{#if !version.is_current}
						<button
							type="button"
							class="underline disabled:opacity-50"
							disabled={busy}
							onclick={() => promote(version)}
						>
							Make current
						</button>
					{/if}
				</li>
			{/each}
		</ol>
		<p class="text-xs text-muted">
			The library and every download resolve to the current version. Older ones stay readable by
			their own link, which is the point of keeping them.
		</p>
	</section>
{/if}
