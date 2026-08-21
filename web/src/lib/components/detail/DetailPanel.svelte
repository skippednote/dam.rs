<!--
	The detail panel: everything about one asset, and the metadata editor.

	**The tier decides what the download control says, and the server decided the tier.** `AssetTier` is
	derived server-side from storage class and restore state precisely so this component does not
	reimplement the rule — the trap the schema warns about twice is that an *expired* restore of an archived
	object is archived again, and conflating it with `restored` leaves a download button enabled until the
	day somebody presses it. Here that shows up as one check: `hot`, `cool` and `restored` can hand over the
	original; `archive` and `restoring` cannot.

	**A rights badge is not a permission.** Enforcement is at the distribution chokepoint (D12); this field
	dims a button and explains why. A UI that treated `allowed` as authorisation would be one stale cache
	away from offering a download the server refuses — so the control is dimmed as a courtesy and the server
	is still the one that says no.
-->
<script lang="ts">
	import type { AssetDetail, Engagement, FieldDefinition } from '$lib/api/client';
	import TierBadge from '$lib/components/state/TierBadge.svelte';
	import RightsBadge from '$lib/components/state/RightsBadge.svelte';
	import ProvenanceBadge from '$lib/components/state/ProvenanceBadge.svelte';
	import AiPanel from './AiPanel.svelte';
	import AssetTypePicker from './AssetTypePicker.svelte';
	import CategoryPanel from './CategoryPanel.svelte';
	import CommentPanel from './CommentPanel.svelte';
	import EngagementPanel from './EngagementPanel.svelte';
	import AttachmentPanel from './AttachmentPanel.svelte';
	import DownloadPanel from './DownloadPanel.svelte';
	import HistoryPanel from './HistoryPanel.svelte';
	import VersionPanel from './VersionPanel.svelte';
	import MetadataEditor from './MetadataEditor.svelte';
	import SharePanel from './SharePanel.svelte';
	import { deliveryUrl } from '$lib/api/client';

	let {
		asset,
		fields,
		onchanged,
		onengagement,
		onversions,
		onclose
	}: {
		asset: AssetDetail;
		/** The tenant's definitions, so the editor knows each field's shape. */
		fields: FieldDefinition[];
		onchanged?: (values: Record<string, unknown>) => void;
		/** So the grid can redraw one cell's star without refetching the page. */
		onengagement?: (state: Engagement) => void;
		/** So the page can reload when a different version becomes current, which changes what the grid shows. */
		onversions?: () => void;
		onclose?: () => void;
	} = $props();

	const values = $derived((asset.values ?? {}) as Record<string, unknown>);
	const technical = $derived((asset.technical ?? {}) as Record<string, unknown>);

	/**
	 * The technical facts as rows, with one level of nesting flattened.
	 *
	 * `technical.embedded` holds everything the file said about itself — camera, lens, exposure, coordinates,
	 * XMP — and `JSON.stringify` on it produced a single unreadable row of two hundred characters. The tags are
	 * the useful part of this panel to a photographer, so they get rows of their own.
	 *
	 * The child key is the label because these keys arrive namespaced (`exif.make`, `xmp.credit`), so it
	 * already reads as what it is; a `embedded.exif.make` label would be three words of scaffolding in front
	 * of the one that matters.
	 */
	const facts = $derived.by(() => {
		const rows: [string, unknown][] = [];
		const nested: [string, unknown][] = [];
		for (const [key, value] of Object.entries(technical)) {
			if (value && typeof value === 'object' && !Array.isArray(value)) {
				nested.push(...Object.entries(value as Record<string, unknown>));
			} else {
				rows.push([key, value]);
			}
		}
		nested.sort(([a], [b]) => a.localeCompare(b));
		return [...rows, ...nested];
	});

	/** Whether the original can be fetched right now. See the component docs. */
	const originalAvailable = $derived(
		asset.tier === 'hot' || asset.tier === 'cool' || asset.tier === 'restored'
	);

	function bytes(n: number): string {
		// Binary units, because that is what the storage layer counts in and a DAM's users compare against
		// what their operating system reports.
		const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}

	function when(iso: string): string {
		// The visible text is locale-formatted and the `datetime` attribute carries the machine-readable
		// value, which is what makes a screen reader and a copy-paste both correct.
		return new Date(iso).toLocaleString();
	}

	/**
	 * The field keys this asset's metadata type includes, or `null` when the tenant has no types.
	 *
	 * Kept here rather than inside the picker because the *editor* is what has to respect it: offering a
	 * field the asset's type excludes produces a save the API refuses, and the refusal lands on an input the
	 * user was invited to fill in.
	 */
	let formKeys = $state<string[] | null>(null);

	const formFields = $derived(
		formKeys === null
			? fields
			: // Ordered by the type, not by the tenant's field order — the whole point of a per-type list.
				formKeys
					.map((key) => fields.find((field) => field.key === key))
					.filter((field): field is FieldDefinition => field !== undefined)
	);
</script>

<!--
	A plain `div`, not a landmark. The page wraps this in an `aside` that is already named "Selected asset",
	and two nested landmarks naming the same thing is announced twice — the panel's own heading is the
	filename, which is the useful name.
-->
<div class="flex h-full flex-col gap-4 overflow-y-auto p-4">
	<header class="space-y-2">
		<div class="flex items-start justify-between gap-2">
			<h2 class="text-sm font-semibold break-all" title={asset.filename}>{asset.filename}</h2>
			{#if onclose}
				<button
					type="button"
					class="shrink-0 rounded p-1 text-muted hover:text-fg"
					onclick={onclose}
					aria-label="Close detail panel"
				>
					✕
				</button>
			{/if}
		</div>

		<div class="flex flex-wrap items-center gap-1.5">
			<TierBadge tier={asset.tier} />
			<RightsBadge state={asset.rights_state} />
			<ProvenanceBadge state={asset.provenance_state} />
			{#if asset.legal_hold}
				<!--
					Surfaced because a user who cannot delete an asset deserves to know why rather than to meet a
					failing button. Legal hold blocks tiering *and* deletion.
				-->
				<span class="rounded bg-surface px-1.5 py-0.5 text-xs font-medium">Legal hold</span>
			{/if}
		</div>
	</header>

	{#if asset.thumbnail_url}
		<!--
			The thumbnail, not the preview: `preview-1024` exists and belongs in a lightbox, and fetching a
			megapixel image to draw it at 288 pixels wide is bandwidth spent on nothing. `alt=""` because the
			filename is the heading directly above — see the grid for the same reasoning.
		-->
		<img
			src={deliveryUrl(asset.thumbnail_url)}
			alt=""
			aria-hidden="true"
			class="image-well w-full rounded-md object-contain"
		/>
	{/if}

	<div class="rounded-md bg-surface p-3">
		{#if originalAvailable}
			<p class="text-xs text-muted">
				The original is available. Downloads pass the rights check at the point of delivery, so this
				may still be refused — the badge above is what it will say.
			</p>
		{:else if asset.tier === 'restoring'}
			<p class="text-xs text-muted">
				A restore is in flight. The proxy stays searchable and previewable while it runs.
			</p>
		{:else}
			<p class="text-xs text-muted">
				The original is in cold storage and needs a restore, which takes between a minute and 48
				hours depending on the class. Search and preview work now.
			</p>
		{/if}
	</div>

	<!-- Directly under the paragraph about availability, because that paragraph is what somebody reads before
	     deciding to take a copy. The panel hides itself for a caller who may look and not download. -->
	<DownloadPanel assetId={asset.id} />

	<dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
		<dt class="text-muted">Type</dt>
		<dd class="font-mono">{asset.mime}</dd>
		<dt class="text-muted">Size</dt>
		<dd class="tabular-nums">{bytes(asset.bytes)}</dd>
		{#if asset.width && asset.height}
			<dt class="text-muted">Dimensions</dt>
			<dd class="tabular-nums">{asset.width} × {asset.height}</dd>
		{/if}
		{#if asset.duration_ms}
			<dt class="text-muted">Duration</dt>
			<dd class="tabular-nums">{(asset.duration_ms / 1000).toFixed(1)} s</dd>
		{/if}
		{#if asset.page_count}
			<dt class="text-muted">Pages</dt>
			<dd class="tabular-nums">{asset.page_count}</dd>
		{/if}
		{#if asset.color_space}
			<dt class="text-muted">Colour</dt>
			<dd>{asset.color_space}</dd>
		{/if}
		<dt class="text-muted">Status</dt>
		<dd>{asset.status}</dd>
		<dt class="text-muted">Enrichment</dt>
		<dd>{asset.enrichment_state}</dd>
		<dt class="text-muted">Version</dt>
		<dd class="tabular-nums">{asset.version_no}</dd>
		<dt class="text-muted">Added</dt>
		<dd><time datetime={asset.created_at}>{when(asset.created_at)}</time></dd>
		{#if asset.expires_at}
			<dt class="text-muted">Expires</dt>
			<dd><time datetime={asset.expires_at}>{when(asset.expires_at)}</time></dd>
		{/if}
		<dt class="text-muted">Hash</dt>
		<!-- Truncated in the middle: the leading and trailing hex are what somebody compares by eye. -->
		<dd class="font-mono text-[10px] break-all">{asset.content_hash}</dd>
	</dl>

	<div class="space-y-2">
		<!-- Which form, before the form itself: a field somebody expects and cannot find is almost always
		     the type, and the editor alone gives them nothing to act on. -->
		<AssetTypePicker assetId={asset.id} onresolved={(keys) => (formKeys = keys)} />
		<h3 class="mb-2 text-xs font-semibold tracking-wide text-muted uppercase">Metadata</h3>
		<MetadataEditor assetId={asset.id} {values} fields={formFields} onsaved={onchanged} />
	</div>

	<!-- Engagement before filing and sharing: it is the cheapest thing a reader does — a star or a favourite is
	     one click and needs no decision, while filing and sharing both do. -->
	<EngagementPanel assetId={asset.id} initial={asset.engagement} onchanged={onengagement} />

	<!-- Where it is filed, before sharing: a reader checking an asset asks "what is this and where does it
	     live" before "who can have it". -->
	<CategoryPanel assetId={asset.id} />

	<!-- The paperwork beside the rights badge in spirit: both answer "may we use this", and the badge alone says
	     what the answer is without saying where it came from. -->
	<AttachmentPanel assetId={asset.id} />

	<!-- The history before the conversation: which bytes you are looking at is a more basic question than what
	     people have said about them, and the panel hides itself entirely for a single-version asset. -->
	<VersionPanel assetId={asset.id} onchanged={onversions} />

	<!-- The conversation after the filing and before sharing: what people have *said* about an asset is part of
	     understanding it, and it belongs beside the metadata rather than behind a tab. -->
	<CommentPanel assetId={asset.id} />

	<SharePanel assetId={asset.id} />

	<!-- Before the history, and closed: which of these words a machine wrote is a question about the asset itself
	     rather than about what has happened to it. Read-gated on the server, deliberately — a marking only
	     administrators can see is not a disclosure (G2). -->
	<AiPanel assetId={asset.id} />

	<!-- Last, and closed: a history is what you consult when something looks wrong, not what you read first. Being a
	     disclosure is also why it costs nothing to have here — it fetches only once somebody opens it. -->
	<HistoryPanel assetId={asset.id} filename={asset.filename} />

	{#if facts.length > 0}
		<details>
			<summary class="cursor-pointer text-xs font-semibold tracking-wide text-muted uppercase">
				Technical
			</summary>
			<!--
				Read-only and shown verbatim. These facts are shaped by the file rather than by the tenant's
				schema, which is why they are not merged into the editable document above.
			-->
			<dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
				{#each facts as [key, value] (key)}
					<dt class="text-muted">{key}</dt>
					<dd class="break-all">
						{typeof value === 'object' ? JSON.stringify(value) : String(value)}
					</dd>
				{/each}
			</dl>
		</details>
	{/if}
</div>
