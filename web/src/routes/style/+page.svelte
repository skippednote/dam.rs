<script lang="ts">
	/**
	 * The token and state reference — every state of every dimension on one page.
	 *
	 * It exists to be scanned by a person *and* by axe: rendering every badge variant here means the
	 * contrast of every token pair is checked on every CI run, which is the only way a tint-plus-hue
	 * combination that measures 3.9:1 gets caught before a customer with low vision finds it.
	 */
	import TierBadge from '$lib/components/state/TierBadge.svelte';
	import RightsBadge from '$lib/components/state/RightsBadge.svelte';
	import ProvenanceBadge from '$lib/components/state/ProvenanceBadge.svelte';
	import ConfidenceBar from '$lib/components/state/ConfidenceBar.svelte';
	import { PROVENANCE_STATES, RIGHTS_STATES, TIERS } from '$lib/components/state/vocabulary';
</script>

<div class="mx-auto max-w-3xl space-y-10 p-8">
	<header>
		<h1 class="text-2xl font-semibold tracking-tight">State vocabulary</h1>
		<p class="mt-2 text-muted">
			Four independent dimensions, four separate perceptual channels. Every variant is rendered here
			so contrast is verified automatically.
		</p>
	</header>

	<section aria-labelledby="tier-heading" class="space-y-3">
		<h2 id="tier-heading" class="text-lg font-medium">Tier — form</h2>
		<p class="text-sm text-muted">
			Glyph and border, one neutral colour. Tier must not compete with rights for the colour
			channel.
		</p>
		<div class="flex flex-wrap gap-2">
			{#each TIERS as tier (tier)}
				<TierBadge {tier} />
			{/each}
		</div>
	</section>

	<section aria-labelledby="rights-heading" class="space-y-3">
		<h2 id="rights-heading" class="text-lg font-medium">Rights — semantic colour</h2>
		<p class="text-sm text-muted">
			The only dimension with legal consequence, so it gets the loudest channel — always with a
			glyph and a label, because colour alone fails WCAG 1.4.1.
		</p>
		<div class="flex flex-wrap gap-2">
			{#each RIGHTS_STATES as state (state)}
				<RightsBadge {state} />
			{/each}
		</div>
	</section>

	<section aria-labelledby="provenance-heading" class="space-y-3">
		<h2 id="provenance-heading" class="text-lg font-medium">Provenance — neutral</h2>
		<p class="text-sm text-muted">
			A missing or broken content credential is a fact about the file's history, not a licensing
			alarm.
		</p>
		<div class="flex flex-wrap gap-2">
			{#each PROVENANCE_STATES as state (state)}
				<ProvenanceBadge {state} />
			{/each}
		</div>
	</section>

	<section aria-labelledby="confidence-heading" class="space-y-3">
		<h2 id="confidence-heading" class="text-lg font-medium">Confidence — magnitude</h2>
		<p class="text-sm text-muted">
			A quantity, so it reads as length — with the value in text, and nothing at all when a tag was
			never scored.
		</p>
		<div class="space-y-2">
			{#each [0.97, 0.62, 0.21, null] as value (value)}
				<div><ConfidenceBar {value} /></div>
			{/each}
		</div>
	</section>
</div>
