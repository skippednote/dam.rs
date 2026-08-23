<script lang="ts">
	/**
	 * Boxes drawn on a picture, and a drag that makes a new one (M6).
	 *
	 * ## The letterbox is the whole difficulty
	 *
	 * The image is `object-contain`, so its element's bounding box includes empty bands on two sides — a 16:9
	 * picture in a 4:3 box has grey above and below. Normalising a drag against the *element* would put every
	 * mark off by the size of those bands, which is the "mark in the wrong place" failure the migration's own
	 * comment warns about. So the rendered content box is computed from `naturalWidth`/`naturalHeight` against
	 * the element's rect, and every coordinate goes through that.
	 *
	 * It has to be recomputed on resize, because the bands change with the container's aspect ratio and a mark
	 * that was right before a window drag would otherwise be wrong after it.
	 *
	 * ## Fractions in, fractions out
	 *
	 * Nothing here stores or emits a pixel. The same annotation is drawn over a thumbnail, a preview and an
	 * original at three different sizes, so the only coordinate that survives is a fraction of the picture.
	 *
	 * ## A drag that goes nowhere is not an annotation
	 *
	 * A click without movement is how somebody dismisses or focuses, not how they draw. Below a few pixels the
	 * drag is discarded — otherwise every stray click would open a comment box over a one-pixel region, and the
	 * server would refuse it anyway for having no extent.
	 */
	let {
		/** Existing annotations, as `[x, y, w, h]` fractions with the id that owns each. */
		regions = [],
		/** Which one is highlighted, if any. */
		selected = null,
		/** Whether a drag may start. False while somebody is reading rather than reviewing. */
		drawing = false,
		onselect,
		ondraw
	}: {
		regions?: { id: string; region: [number, number, number, number]; label?: string }[];
		selected?: string | null;
		drawing?: boolean;
		onselect?: (id: string | null) => void;
		ondraw?: (region: [number, number, number, number]) => void;
	} = $props();

	/** The smallest drag, in pixels of the rendered image, that counts as drawing rather than clicking. */
	const MIN_DRAG = 6;

	let host = $state<HTMLDivElement | null>(null);
	/** The rendered content box of the image inside `host`, in host-relative pixels. */
	let content = $state({ left: 0, top: 0, width: 0, height: 0 });
	/** The drag in progress, in host-relative pixels. */
	let drag = $state<{ x: number; y: number; w: number; h: number } | null>(null);
	let anchorPoint = $state<{ x: number; y: number } | null>(null);

	/**
	 * Measures where the picture actually is inside its element.
	 *
	 * `object-contain` scales to fit and centres, so the content box is the element's box shrunk on one axis by
	 * the ratio of the two aspect ratios. Without the natural size there is nothing to compute from — an image
	 * that has not loaded yet reports 0, and the overlay stays empty rather than guessing.
	 */
	function measure() {
		if (!host) return;
		const image = host.parentElement?.querySelector('img');
		if (!(image instanceof HTMLImageElement) || !image.naturalWidth || !image.naturalHeight) {
			content = { left: 0, top: 0, width: 0, height: 0 };
			return;
		}
		const box = image.getBoundingClientRect();
		const hostBox = host.getBoundingClientRect();
		const scale = Math.min(box.width / image.naturalWidth, box.height / image.naturalHeight);
		const width = image.naturalWidth * scale;
		const height = image.naturalHeight * scale;
		content = {
			// Relative to the overlay, so the boxes below need no further offset.
			left: box.left - hostBox.left + (box.width - width) / 2,
			top: box.top - hostBox.top + (box.height - height) / 2,
			width,
			height
		};
	}

	$effect(() => {
		measure();
		const observer = new ResizeObserver(measure);
		// **The image, not just the container.** Watching only the container was a real bug, and a test that
		// forced letterboxing caught it: the image can change size without the container doing so — responsive
		// CSS, a stylesheet arriving late, a different asset with another aspect ratio — and a stale
		// measurement puts every coordinate off by the bands. It clamped a drag to the left edge, which reads
		// as "the overlay is broken" rather than "the measurement is old".
		const image = host?.parentElement?.querySelector('img');
		if (image) observer.observe(image);
		// The container too: under `max-w-full` the image resizes *because* the container did, and observing
		// only the image would miss the frame where the container has changed and the image has not yet.
		if (host?.parentElement) observer.observe(host.parentElement);
		// And the natural size is unknown until the image loads, which is usually after this runs.
		image?.addEventListener('load', measure);
		return () => {
			observer.disconnect();
			image?.removeEventListener('load', measure);
		};
	});

	/** A point in host pixels, as a fraction of the picture — clamped, so a drag off the edge stops at it. */
	function fractionOf(x: number, y: number): { x: number; y: number } {
		if (content.width === 0 || content.height === 0) return { x: 0, y: 0 };
		return {
			x: Math.min(1, Math.max(0, (x - content.left) / content.width)),
			y: Math.min(1, Math.max(0, (y - content.top) / content.height))
		};
	}

	function start(event: PointerEvent) {
		if (!drawing || !host) return;
		const box = host.getBoundingClientRect();
		anchorPoint = { x: event.clientX - box.left, y: event.clientY - box.top };
		drag = { x: anchorPoint.x, y: anchorPoint.y, w: 0, h: 0 };
		// Captured, so a drag that leaves the image still ends here rather than being lost to whatever it
		// passes over.
		host.setPointerCapture(event.pointerId);
	}

	function move(event: PointerEvent) {
		if (!anchorPoint || !host) return;
		const box = host.getBoundingClientRect();
		const x = event.clientX - box.left;
		const y = event.clientY - box.top;
		drag = {
			x: Math.min(anchorPoint.x, x),
			y: Math.min(anchorPoint.y, y),
			w: Math.abs(x - anchorPoint.x),
			h: Math.abs(y - anchorPoint.y)
		};
	}

	function finish() {
		const current = drag;
		anchorPoint = null;
		drag = null;
		if (!current) return;
		// A click, not a drag. Treated as "deselect" rather than as a zero-size annotation.
		if (current.w < MIN_DRAG || current.h < MIN_DRAG) {
			onselect?.(null);
			return;
		}
		const from = fractionOf(current.x, current.y);
		const to = fractionOf(current.x + current.w, current.y + current.h);
		const region: [number, number, number, number] = [from.x, from.y, to.x - from.x, to.y - from.y];
		// Clamped to the picture above, so this can only fail if the image has not loaded — in which case
		// there is nothing to annotate yet.
		if (region[2] <= 0 || region[3] <= 0) return;
		ondraw?.(region);
	}

	/** An existing region as host-relative pixels. */
	function place(region: [number, number, number, number]) {
		return {
			left: content.left + region[0] * content.width,
			top: content.top + region[1] * content.height,
			width: region[2] * content.width,
			height: region[3] * content.height
		};
	}
</script>

<!--
	`inset-0` over the image's container, and pointer events only while drawing — otherwise the overlay would
	swallow every click meant for the lightbox behind it.

	`role="group"` with a label, and the accessibility position stated plainly rather than hidden behind a
	role that claims more: the *existing* regions inside this are buttons, so reading and focusing annotations
	works from the keyboard. **Drawing a new one does not.** A drag is inherently a pointer gesture, and no
	role attribute changes that.

	`role="application"` would silence the linter and be a lie — it tells a screen reader to stop intercepting
	keys, which would make this worse for the very users it appears to help. So the group is honest about what
	it is, and the missing keyboard path for *creating* an annotation is recorded as a gap in TASKS.md rather
	than papered over. The nearest fix is a control that annotates a named region ("top left", "whole image")
	without a drag, which is a design question rather than an attribute.
-->
<div
	bind:this={host}
	role="group"
	aria-label={drawing
		? 'Annotated regions. Drag on the picture to add one — this requires a pointer.'
		: 'Annotated regions'}
	class="absolute inset-0 {drawing ? 'cursor-crosshair' : 'pointer-events-none'}"
	onpointerdown={start}
	onpointermove={move}
	onpointerup={finish}
	onpointercancel={finish}
>
	{#each regions as mark (mark.id)}
		{@const box = place(mark.region)}
		<!--
			A button, not a div: an annotation is a control that focuses its comment, so it has to be reachable
			by keyboard and announced as something activatable. `pointer-events-auto` because the host disables
			them while not drawing.
		-->
		<button
			type="button"
			class="pointer-events-auto absolute rounded-sm border-2 transition-colors {selected ===
			mark.id
				? 'border-accent bg-accent/10'
				: 'border-fg/70 bg-fg/5 hover:border-accent'}"
			style="left: {box.left}px; top: {box.top}px; width: {box.width}px; height: {box.height}px"
			aria-label={mark.label ?? 'Annotated region'}
			aria-pressed={selected === mark.id}
			onclick={(event) => {
				event.stopPropagation();
				onselect?.(mark.id);
			}}
		></button>
	{/each}

	{#if drag && (drag.w >= MIN_DRAG || drag.h >= MIN_DRAG)}
		<!-- The drag in progress. Not a control: it exists for the duration of a gesture. -->
		<div
			aria-hidden="true"
			class="absolute rounded-sm border-2 border-dashed border-accent bg-accent/10"
			style="left: {drag.x}px; top: {drag.y}px; width: {drag.w}px; height: {drag.h}px"
		></div>
	{/if}
</div>
