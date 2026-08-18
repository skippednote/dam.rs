<!--
	The application navigation.

	`aria-current="page"` rather than a class alone: a visual highlight tells a sighted user where they are
	and tells a screen-reader user nothing. It is one attribute and it is the whole difference between a nav
	that works and one that looks like it does.
-->
<script lang="ts">
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import { session } from '$lib/api/session.svelte';

	// `resolve` rather than a bare string: it applies the base path and, more usefully, makes an internal
	// link a *checked* reference — a route renamed or deleted becomes a type error rather than a 404 nobody
	// clicks until after release.
	const LINKS = [
		{ href: resolve('/assets'), label: 'Assets' },
		{ href: resolve('/style'), label: 'Style' },
		{ href: resolve('/settings'), label: 'Settings' }
	];

	function current(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(`${href}/`);
	}
</script>

<nav aria-label="Main" class="flex h-12 items-center gap-1 border-b border-line px-4">
	<a href={resolve('/')} class="mr-3 text-sm font-semibold tracking-tight">damrs</a>

	{#each LINKS as link (link.href)}
		<a
			href={link.href}
			aria-current={current(link.href) ? 'page' : undefined}
			class="rounded-md px-2.5 py-1 text-sm {current(link.href)
				? 'bg-surface font-medium'
				: 'text-muted hover:text-fg'}"
		>
			{link.label}
		</a>
	{/each}

	<span class="ml-auto text-xs">
		{#if session.connected}
			<!-- The prefix only. It is what an audit log shows, so displaying it discloses nothing new. -->
			<span class="font-mono text-muted">{session.visible}</span>
		{:else}
			<a href={resolve('/settings')} class="text-state-rights-denied-fg underline">Not connected</a>
		{/if}
	</span>
</nav>
