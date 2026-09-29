<script lang="ts">
	import DocsPage from '$lib/components/docs/DocsPage.svelte';
	import type { DocSlug } from '$lib/docs/content';

	let { data }: { data: { slug: DocSlug } } = $props();
</script>

<DocsPage slug={data.slug} />
