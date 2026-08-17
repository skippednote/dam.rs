<!--
	Provenance, neutral by design. A broken content credential is a fact about the file's history, not
	a licensing alarm — painting it in the rights palette sends the wrong person to investigate.
-->
<script lang="ts">
	import { provenanceMeta, type ProvenanceState } from './vocabulary';

	let { state }: { state: ProvenanceState } = $props();
	const meta = $derived(provenanceMeta(state));
</script>

<span
	data-testid="provenance-badge"
	data-provenance={state}
	title={meta.detail}
	class="inline-flex items-center gap-1.5 rounded-md border border-state-neutral px-2 py-0.5
	       text-xs text-muted"
>
	<span aria-hidden="true">{meta.icon}</span>
	<span>{meta.label}</span>
</span>
