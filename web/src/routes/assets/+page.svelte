<script lang="ts">
	/**
	 * The asset browser. Sample data until the API layer lands — the endpoints are stopped pending the
	 * authentication decisions in NEEDS-REVIEW.md, and this page exists so the grid's ARIA semantics and
	 * keyboard behaviour are exercised in a real browser by CI rather than only in component tests.
	 */
	import AssetGrid from '$lib/components/grid/AssetGrid.svelte';
	import type { AssetSummary } from '$lib/components/grid/types';

	const TIERS = ['hot', 'cool', 'archive', 'restoring', 'restored'] as const;
	const RIGHTS = ['allowed', 'expiring', 'denied', 'unknown'] as const;
	const PROVENANCE = ['valid', 'none', 'invalid', 'untrusted'] as const;

	const items: AssetSummary[] = Array.from({ length: 120 }, (_, i) => ({
		id: `00000000-0000-0000-0000-${String(i).padStart(12, '0')}`,
		filename: `campaign-${String(i).padStart(4, '0')}.jpg`,
		mime: 'image/jpeg',
		bytes: 2_400_000 + i * 1024,
		width: 4000,
		height: 3000,
		tier: TIERS[i % TIERS.length],
		rights_state: RIGHTS[i % RIGHTS.length],
		provenance_state: PROVENANCE[i % PROVENANCE.length],
		thumbnail_url: null,
		tag_confidence: i % 3 === 0 ? null : 0.55 + (i % 40) / 100
	}));
</script>

<div class="mx-auto max-w-5xl space-y-4 p-8">
	<h1 class="text-2xl font-semibold tracking-tight">Assets</h1>
	<p class="text-muted">
		Arrow keys move between cells, Home and End jump within a row, and ctrl with either jumps to the
		start or end of the collection. Click to select, shift-click for a range, and hold ⌘ or Ctrl to
		toggle.
	</p>
	<AssetGrid {items} total={items.length} columns={4} height={520} rowHeight={110} />
</div>
