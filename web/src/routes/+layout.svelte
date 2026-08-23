<script lang="ts">
	import './layout.css';
	import { page } from '$app/state';
	import SkipLink from '$lib/components/a11y/SkipLink.svelte';
	import Nav from '$lib/components/shell/Nav.svelte';
	import brandMark from '$lib/assets/damrs-mark.png';

	let { children } = $props();

	// A page sets its own title through `load`; this is the fallback. Without one, axe reports
	// `doc-has-title` (WCAG 2.4.2) — which is how this was found: the SvelteKit scaffold ships
	// without a title, so every page would be announced by its URL.
	const title = $derived(page.data.title ? `${page.data.title} · dam.rs` : 'dam.rs');
	const portal = $derived(
		page.url.pathname.startsWith('/share/') || page.url.pathname.startsWith('/portal/')
	);
</script>

<svelte:head>
	<link rel="icon" href={brandMark} />
	<title>{title}</title>
	<meta
		name="description"
		content="Rights-aware digital asset management. Find it. Trust it. Use it."
	/>
	<meta name="theme-color" content="#11151d" />
</svelte:head>

<SkipLink />

<Nav />

<!--
	`tabindex="-1"` is what makes the skip link work. Without it the browser scrolls to the landmark
	but leaves focus in the header, so the next Tab continues from where it was — a screen-reader user
	is told they have skipped and then discovers they have not.
-->
<main id="main-content" tabindex="-1" class:app-main={!portal}>
	{@render children()}
</main>
