<script lang="ts">
	import './layout.css';
	import { page } from '$app/state';
	import { isPublicRoute } from '$lib/chrome';
	import SkipLink from '$lib/components/a11y/SkipLink.svelte';
	import Nav from '$lib/components/shell/Nav.svelte';
	import favicon from '$lib/assets/favicon.svg';

	let { children } = $props();

	// A page sets its own title through `load`; this is the fallback. Without one, axe reports
	// `doc-has-title` (WCAG 2.4.2) — which is how this was found: the SvelteKit scaffold ships
	// without a title, so every page would be announced by its URL.
	const title = $derived(page.data.title ? `${page.data.title} · dam.rs` : 'dam.rs');
	const portal = $derived(isPublicRoute(page.url.pathname));
</script>

<svelte:head>
	<!--
		A square icon, not the rail's mark: a favicon is rendered into a square, so letterboxing a portrait
		mark leaves it smaller than the space allows. `type` so the browser does not have to sniff.
	-->
	<link rel="icon" type="image/svg+xml" href={favicon} />
	<title>{title}</title>
	<meta
		name="description"
		content="Rights-aware digital asset management. Find it. Trust it. Use it."
	/>
	<!--
		Two, because one is a promise about a theme this application lets you change. A single dark hex left
		the browser chrome dark above a light page — most visible on iOS, where the status bar is the thing
		it colours. Both hexes are `--color-bg` as the browser actually renders it, read off the page rather
		than converted from the oklch by hand.
	-->
	<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#0e0f13" />
	<meta name="theme-color" media="(prefers-color-scheme: light)" content="#fbfcfd" />
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
