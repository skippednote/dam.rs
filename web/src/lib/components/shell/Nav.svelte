<!--
	The application navigation.

	`aria-current="page"` rather than a class alone: a visual highlight tells a sighted user where they are
	and tells a screen-reader user nothing. It is one attribute and it is the whole difference between a nav
	that works and one that looks like it does.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import { branding } from '$lib/api/branding.svelte';
	import { session } from '$lib/api/session.svelte';

	// `resolve` rather than a bare string: it applies the base path and, more usefully, makes an internal
	// link a *checked* reference — a route renamed or deleted becomes a type error rather than a 404 nobody
	// clicks until after release.
	const LINKS = [
		{ href: resolve('/assets'), label: 'Assets' },
		// The two private lists sit next to Assets because that is what they are — the library, filtered to what
		// this person marked. Further from Assets they would read as separate features rather than as views of it.
		{ href: resolve('/favourites'), label: 'Favourites' },
		{ href: resolve('/watches'), label: 'Watching' },
		{ href: resolve('/shares'), label: 'Shares' },
		// Between Shares and Orders because that is the neighbourhood it belongs to: a collection is what a
		// portal publishes, and a portal is a share with a front door.
		{ href: resolve('/collections'), label: 'Collections' },
		{ href: resolve('/orders'), label: 'Orders' },
		{ href: resolve('/schema'), label: 'Schema' },
		{ href: resolve('/storage'), label: 'Storage' },
		// Beside Review because both are queues of work over the library, and a person who opens one has
		// reason to open the other.
		{ href: resolve('/worklists'), label: 'Worklists' },
		// Beside Worklists because it is the same kind of thing: a queue of judgements the library needs from
		// a person, computed from what it already holds.
		{ href: resolve('/duplicates'), label: 'Duplicates' },
		// And beside those two for the third time, with one difference worth the placement: a worklist and a
		// duplicate queue are computed, while a round is somebody having *asked*. It sits with them because a
		// reviewer opening the application to see what is waiting on them should find all three together.
		{ href: resolve('/proofing'), label: 'Proofing' },
		// After the queues rather than beside Settings: Insights is read-only and about the library, not about
		// how the library is configured. It is the one screen here that answers "is any of this being used".
		{ href: resolve('/insights'), label: 'Insights' },
		// Next to Schema because it is the same kind of work: deciding what the library's own vocabulary means.
		{ href: resolve('/review'), label: 'Review' },
		// Beside Review for the same reason, and because the review queue is where a badly-set threshold shows
		// up: the terms a model proposes come from here.
		{ href: resolve('/vocabularies'), label: 'Vocabularies' },
		// After the content screens: a webhook is how the library talks to everything outside it, which is
		// closer to Settings than to anything a curator opens daily.
		{ href: resolve('/webhooks'), label: 'Webhooks' },
		// Beside Webhooks for the same reason, and because they are the two halves of one thing: a webhook is
		// how the library tells another system something changed, and a connected site is that system.
		{ href: resolve('/connectors'), label: 'Sites' },
		// Between Sites and Settings: it is administration rather than content, and it is the screen an auditor
		// is pointed at — which is a different visit from a curator's, so it does not belong among the queues.
		{ href: resolve('/governance'), label: 'Governance' },
		{ href: resolve('/style'), label: 'Style' },
		// Beside Settings, because it is configuration rather than content — and it is the one screen where a
		// tenant makes the application look like theirs.
		{ href: resolve('/branding'), label: 'Branding' },
		{ href: resolve('/settings'), label: 'Settings' }
	];

	function current(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(`${href}/`);
	}

	// Loaded here because this is the one component on every route. The store guards against repeat calls, so
	// mounting it per navigation costs nothing after the first.
	onMount(() => {
		void branding.ensure();
	});
	/**
	 * A portal has no nav: its visitor is an external recipient with no account, and an app chrome saying "Not
	 * connected" invites them to try to connect to something that was never theirs.
	 *
	 * Both addresses, which the browser suite caught: Q.14's named portals live under `/portal/`, and adding the
	 * page without adding it here put the application's own navigation on a page meant for a tenant's customers.
	 */
	const portal = $derived(
		page.url.pathname.startsWith('/share/') || page.url.pathname.startsWith('/portal/')
	);
</script>

{#if !portal}
	<!--
		Wrapping, and a minimum height rather than a fixed one.

		Eighteen sections do not fit one row: measured, the nav wants 1461px, so at 1024 and 1280 — an iPad in
		landscape and the default MacBook Air — the last item sat outside the viewport entirely, unreachable
		by mouse. I added six of those sections over this session without once checking the row still fitted,
		which is exactly the kind of thing that only shows up in a screenshot.

		Wrapping is the honest minimum: nothing becomes unreachable and it degrades predictably at every
		width. It is not the right *design* — eighteen flat sections wants grouping, with the configuration
		half (Schema, Vocabularies, Storage, Webhooks, Sites, Branding, Style) under Settings and the eleven daily
		ones on top — but that is a routing change, and a nav that clips is a bug while a nav that wraps is
		only plain.
	-->
	<nav
		aria-label="Main"
		class="flex min-h-12 flex-wrap items-center gap-x-1 gap-y-0.5 border-b border-line px-4 py-1"
	>
		<!--
			The tenant's own name, and **nothing** until it is known.

			The first version fell back to "damrs", which put a flash of the vendor's name into every page load
			of every customer's library — the exact thing this feature exists to remove, just briefly. Driving
			the real page is what showed it: "immediately: damrs | settled: Acme Picture Library". A blank word
			for one request is less wrong than momentarily claiming to be a different product, and the mark
			below keeps the link present and clickable while it resolves.

			The accent is a **decorative mark**, not the app's `--color-accent`. That token has separately tuned
			light and dark foreground pairs, and `layout.css` says why: "a light-on-light-blue button is the
			classic 3:1 failure". Substituting an arbitrary tenant hex would break those pairs on surfaces that
			carry text, and axe would catch some of it but not all. A bar carries no text, so it cannot fail a
			contrast check — and the accent's real job is the *portal*, which is external-facing and already
			renders it in full.
		-->
		<a
			href={resolve('/')}
			class="mr-3 flex items-center gap-2 text-sm font-semibold tracking-tight"
		>
			{#if branding.logoUrl}
				<img src={branding.logoUrl} alt="" width="20" height="20" class="h-5 w-5 object-contain" />
			{:else}
				<!-- Decorative, and always present: it keeps the home link a clickable target while the name
				     resolves, and it carries no text so it cannot fail a contrast check. -->
				<span
					aria-hidden="true"
					class="h-4 w-1 rounded-full"
					style="background-color: {branding.accent}"
				></span>
			{/if}
			{#if branding.name}
				{branding.name}
			{:else}
				<!--
					A link needs discernible text, always. Removing the "damrs" fallback to stop the vendor-name
					flash left this link with *no* accessible name — the mark beside it is `aria-hidden`, so a
					screen reader heard nothing at all, on every page of the application. axe called it
					`link-name (serious)` across every screen, which is how a visual fix turns into a worse
					accessibility bug than the thing it fixed.

					So the name is announced even while it is unknown: invisible, so no vendor name appears on
					screen, and present, so the link is never nameless.
				-->
				<span class="sr-only">Home</span>
			{/if}
		</a>

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
				<a href={resolve('/settings')} class="text-state-rights-denied-fg underline"
					>Not connected</a
				>
			{/if}
		</span>
	</nav>
{/if}
