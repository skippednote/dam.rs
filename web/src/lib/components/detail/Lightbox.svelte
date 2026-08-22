<!--
	The lightbox: one asset, as large as the viewport allows.

	## It is a modal dialog, and the platform already knows how to be one

	`<dialog showModal()>` gives the focus trap, the inert background, `Escape`, the top layer and the
	`::backdrop` — every one of which is a thing hand-rolled modals get wrong. The most common failure is the
	focus trap: a div-based modal leaves the page behind it tabbable, so a keyboard user tabs straight out of
	the "modal" and operates a UI they cannot see.

	The one thing `<dialog>` does not do is restore focus reliably across frameworks, so that is explicit.

	## `preview-1024`, not the original and not the thumbnail

	The thumbnail is 256px square and cropped — enlarging it is a blurry crop of the wrong aspect. The
	original may be a 200 MB TIFF the browser cannot decode, and fetching it to look at would spend the
	customer's egress on a glance. `preview-1024` is `Contain`-fitted, so nothing is cropped out of an image
	somebody is inspecting, which is exactly what it was defined for.

	And it is fetched through the same signed chokepoint as everything else, with the internal-preview purpose
	— see `dam_core::signed_url::Purpose`. A lightbox is looking, not distribution.

	## Arrow keys move between assets without closing

	Reviewing a shoot means looking at forty frames in a row. A lightbox that has to be closed and reopened
	per frame is one nobody uses; `←`/`→` are the whole interaction.
-->
<script lang="ts">
	import type { AssetDetail } from '$lib/api/client';
	import { deliveryUrl } from '$lib/api/client';
	import TierBadge from '$lib/components/state/TierBadge.svelte';
	import RightsBadge from '$lib/components/state/RightsBadge.svelte';
	import ProvenanceBadge from '$lib/components/state/ProvenanceBadge.svelte';

	let {
		asset,
		/** Whether stepping is possible, so the controls can say so rather than failing silently. */
		hasPrevious = false,
		hasNext = false,
		onclose,
		onprevious,
		onnext
	}: {
		asset: AssetDetail;
		hasPrevious?: boolean;
		hasNext?: boolean;
		onclose: () => void;
		onprevious?: () => void;
		onnext?: () => void;
	} = $props();

	let dialog = $state<HTMLDialogElement | null>(null);
	/** Where focus was before the dialog opened, so it can go back. */
	let returnTo: HTMLElement | null = null;

	$effect(() => {
		const element = dialog;
		if (!element) return;
		returnTo = document.activeElement as HTMLElement | null;
		// `showModal`, not the `open` attribute: only the method puts the dialog in the top layer, makes the
		// rest of the document inert, and enables `::backdrop`. Setting `open` gives a non-modal dialog that
		// looks identical and traps nothing.
		if (!element.open) element.showModal();
		return () => {
			if (element.open) element.close();
			// Explicit, because `<dialog>`'s own focus restoration does not survive the element being
			// destroyed by the framework rather than closed by the user — which is what happens here, since
			// the parent unmounts it.
			returnTo?.focus?.();
		};
	});

	/** The preview, or the thumbnail if that is all this asset has. */
	const source = $derived(asset.preview_url ?? asset.thumbnail_url);

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowLeft' && hasPrevious) {
			event.preventDefault();
			onprevious?.();
		} else if (event.key === 'ArrowRight' && hasNext) {
			event.preventDefault();
			onnext?.();
		}
		// `Escape` is deliberately not handled: `<dialog>` fires `cancel`/`close` for it, and handling it here
		// as well would close twice.
	}

	function bytes(n: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}
</script>

<!--
	`aria-label` on the dialog: it is the accessible name of the whole modal, and a dialog with no name is
	announced as "dialog" with no indication of what opened.
-->
<dialog
	bind:this={dialog}
	aria-label={`Preview of ${asset.filename}`}
	class="m-0 h-full max-h-none w-full max-w-none bg-bg/95 p-0 text-fg backdrop:bg-black/70"
	{onclose}
	{onkeydown}
>
	<div class="flex h-full flex-col">
		<header class="flex shrink-0 items-center gap-3 border-b border-line px-4 py-3">
			<h2 class="min-w-0 flex-1 truncate text-sm font-semibold" title={asset.filename}>
				{asset.filename}
			</h2>
			<span class="flex shrink-0 flex-wrap items-center gap-1.5">
				<TierBadge tier={asset.tier} />
				<RightsBadge state={asset.rights_state} />
				<ProvenanceBadge state={asset.provenance_state} />
			</span>
			<button
				type="button"
				class="shrink-0 rounded-md px-2 py-1 text-sm text-muted hover:bg-raised hover:text-fg"
				onclick={onclose}
			>
				Close
				<!-- The shortcut is shown, because a modal whose only exit is a mouse is a modal that traps. -->
				<kbd class="ml-1 font-mono text-xs text-muted">Esc</kbd>
			</button>
		</header>

		<div class="relative flex min-h-0 flex-1 items-center justify-center p-4">
			{#if source}
				<!--
					`object-contain` and bounded by the box: an image larger than the viewport must not scroll the
					dialog, and one smaller must not be stretched — a 400px logo blown up to 1400px looks broken
					in a way that reads as our fault rather than the file's.

					`alt` is the filename here, unlike the grid: this *is* the content of the dialog, so a screen
					reader arriving at it needs to be told what it is.
				-->
				<img
					src={deliveryUrl(source)}
					alt={asset.filename}
					class="image-well max-h-full max-w-full object-contain"
					decoding="async"
				/>
			{:else}
				<p class="max-w-sm text-center text-sm text-muted">
					{#if asset.tier === 'archive'}
						This asset's original is in cold storage and no preview has been rendered. Search and
						metadata work; a restore is needed to see the image.
					{:else}
						No preview yet — the worker renders one shortly after upload. Formats with no image
						rendition, like a spreadsheet, never get one.
					{/if}
				</p>
			{/if}

			{#if hasPrevious}
				<button
					type="button"
					class="absolute top-1/2 left-4 -translate-y-1/2 rounded-full bg-surface/90 px-3 py-2 text-lg
					       shadow-lg hover:bg-raised"
					onclick={onprevious}
					aria-label="Previous asset"
				>
					‹
				</button>
			{/if}
			{#if hasNext}
				<button
					type="button"
					class="absolute top-1/2 right-4 -translate-y-1/2 rounded-full bg-surface/90 px-3 py-2 text-lg
					       shadow-lg hover:bg-raised"
					onclick={onnext}
					aria-label="Next asset"
				>
					›
				</button>
			{/if}
		</div>

		<footer
			class="flex shrink-0 flex-wrap items-center gap-x-6 gap-y-1 border-t border-line px-4 py-2 text-xs
			       text-muted"
		>
			<span class="font-mono">{asset.mime}</span>
			<span class="tabular">{bytes(asset.bytes)}</span>
			{#if asset.width && asset.height}
				<span class="tabular">{asset.width} × {asset.height}</span>
			{/if}
			{#if asset.color_space}<span>{asset.color_space}</span>{/if}
			<span class="ml-auto">
				<kbd class="font-mono">←</kbd> <kbd class="font-mono">→</kbd> to move between assets
			</span>
		</footer>
	</div>
</dialog>
