<script lang="ts">
	// The scaffold's landing page, replaced with something that exercises the stack the milestone
	// actually specifies: Svelte 5 runes, Tailwind 4 tokens, and a bits-ui primitive.
	import { Accordion } from 'bits-ui';

	let uploads = $state(0);
</script>

<div class="mx-auto max-w-3xl p-8">
	<h1 class="text-2xl font-semibold tracking-tight">damrs</h1>
	<p class="mt-2 text-muted">
		Digital asset management. The UI shell — the asset grid, filter rail and upload queue land in
		F.4 and beyond.
	</p>

	<!-- Proves runes are compiled and reactive, which is what D9 chose Svelte for. -->
	<button
		class="mt-6 rounded-md bg-accent px-3 py-2 text-sm font-medium text-white"
		onclick={() => (uploads += 1)}
	>
		Queued uploads: {uploads}
	</button>

	<!--
		A bits-ui primitive, present to prove the WAI-ARIA layer §14.1 relies on is wired up: the
		accordion manages `aria-expanded`, `aria-controls` and roving focus itself, which is precisely
		the work D9 declined to hand-roll.
	-->
	<Accordion.Root type="single" class="mt-8">
		<Accordion.Item value="stack">
			<Accordion.Header>
				<Accordion.Trigger class="w-full rounded-md bg-surface px-3 py-2 text-left text-sm">
					What is in this scaffold?
				</Accordion.Trigger>
			</Accordion.Header>
			<Accordion.Content class="px-3 py-2 text-sm text-muted">
				SvelteKit with Svelte 5 runes, Tailwind 4, bits-ui primitives, Vitest in browser mode,
				Playwright, and axe-core wired as a merge gate.
			</Accordion.Content>
		</Accordion.Item>
	</Accordion.Root>
</div>
