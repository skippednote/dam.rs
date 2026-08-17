<!--
	Confidence, as a magnitude.
	A bar is a `div` with a width unless it says otherwise, and a `div` with a width is invisible to a
	screen reader — hence `role="meter"` and the value attributes. The percentage is also rendered as
	text, which is what a screen reader and a monochrome printout get.
-->
<script lang="ts">
	import { clampConfidence, confidenceLabel } from './vocabulary';

	let { value, label = 'Model confidence' }: { value: number | null; label?: string } = $props();
	const clamped = $derived(clampConfidence(value));
	const percent = $derived(clamped === null ? null : Math.round(clamped * 100));
</script>

{#if percent === null}
	<!-- No meter at all rather than a meter reading zero: an empty bar would claim the model was
	     certain the tag was wrong, when in fact nothing scored it. -->
	<span class="text-xs text-muted">{confidenceLabel(value)}</span>
{:else}
	<span class="inline-flex items-center gap-2">
		<span
			role="meter"
			aria-label={label}
			aria-valuenow={percent}
			aria-valuemin="0"
			aria-valuemax="100"
			aria-valuetext={confidenceLabel(value)}
			class="inline-block h-1.5 w-24 overflow-hidden rounded-full bg-surface"
		>
			<span class="block h-full rounded-full bg-accent" style="width: {percent}%" aria-hidden="true"
			></span>
		</span>
		<span class="text-xs text-muted tabular-nums">{percent}%</span>
	</span>
{/if}
