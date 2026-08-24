<!--
	Rights, in semantic colour — plus a glyph and a label, because colour alone fails WCAG 1.4.1 and
	because the person who most needs this indicator may be the one who cannot distinguish amber from
	green.
-->
<script lang="ts">
	import { rightsMeta, type RightsState } from './vocabulary';
	import { CheckCircle, ClockCountdown, Prohibit, Question } from 'phosphor-svelte';

	let { state }: { state: RightsState } = $props();
	const meta = $derived(rightsMeta(state));

	// Explicit per state rather than interpolated: Tailwind cannot see a class name built at runtime,
	// so `bg-{token}` would be purged from the build and every badge would render unstyled.
	const TONE: Record<RightsState, string> = {
		allowed: 'bg-state-rights-allowed/18 text-state-rights-allowed-fg',
		expiring: 'bg-state-rights-expiring/18 text-state-rights-expiring-fg',
		denied: 'bg-state-rights-denied/18 text-state-rights-denied-fg',
		unknown: 'bg-state-rights-unknown/18 text-state-rights-unknown-fg'
	};
</script>

<span
	data-testid="rights-badge"
	data-rights={state}
	data-blocks-distribution={meta.blocksDistribution}
	title={meta.detail}
	class="inline-flex items-center gap-1.5 rounded-md px-2 py-0.5 text-xs font-medium {TONE[state]}"
>
	{#if state === 'allowed'}
		<CheckCircle size={14} weight="fill" aria-hidden="true" />
	{:else if state === 'expiring'}
		<ClockCountdown size={14} weight="fill" aria-hidden="true" />
	{:else if state === 'denied'}
		<Prohibit size={14} weight="fill" aria-hidden="true" />
	{:else}
		<Question size={14} weight="bold" aria-hidden="true" />
	{/if}
	<span>{meta.label}</span>
</span>
