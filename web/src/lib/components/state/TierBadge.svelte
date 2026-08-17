<!--
	Tier, encoded in form. Every tier shares one neutral colour token and differs by glyph and border,
	so the colour channel stays free for rights — the only dimension with legal consequence.
-->
<script lang="ts">
	import { tierMeta, type Tier } from './vocabulary';

	let { tier }: { tier: Tier } = $props();
	const meta = $derived(tierMeta(tier));

	const BORDER: Record<string, string> = {
		solid: 'border border-solid',
		dashed: 'border border-dashed',
		dotted: 'border border-dotted',
		double: 'border-4 border-double',
		none: 'border-0'
	};
</script>

<span
	data-testid="tier-badge"
	data-tier={tier}
	data-needs-restore={meta.needsRestore}
	title={meta.detail}
	class="inline-flex items-center gap-1.5 rounded-md border-state-neutral bg-surface px-2 py-0.5
	       text-xs text-fg {BORDER[meta.border]}"
>
	<!-- Decorative: the label already carries the meaning, so a screen reader must not read the glyph
	     as "dotted circle Archived". -->
	<span data-testid="tier-glyph" aria-hidden="true">{meta.icon}</span>
	<span>{meta.label}</span>
</span>
