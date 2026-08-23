<!--
	Provenance, neutral by design. A broken content credential is a fact about the file's history, not
	a licensing alarm — painting it in the rights palette sends the wrong person to investigate.
-->
<script lang="ts">
	import { provenanceMeta, type ProvenanceState } from './vocabulary';
	import { MinusCircle, SealCheck, ShieldWarning, Warning } from 'phosphor-svelte';

	let { state }: { state: ProvenanceState } = $props();
	const meta = $derived(provenanceMeta(state));
</script>

<span
	data-testid="provenance-badge"
	data-provenance={state}
	title={meta.detail}
	class="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-0.5
	       text-xs text-muted"
>
	{#if state === 'valid'}
		<SealCheck size={14} aria-hidden="true" />
	{:else if state === 'invalid'}
		<Warning size={14} aria-hidden="true" />
	{:else if state === 'untrusted'}
		<ShieldWarning size={14} aria-hidden="true" />
	{:else}
		<MinusCircle size={14} aria-hidden="true" />
	{/if}
	<span>{meta.label}</span>
</span>
