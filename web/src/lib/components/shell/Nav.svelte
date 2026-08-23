<!--
	The product rail: a small number of durable neighbourhoods rather than one flat list.

	The application had outgrown its horizontal header: twelve equal-weight destinations left less room for
	the asset work itself and made governance look as frequent as browsing. Grouping preserves every route while
	making the operator's mental model visible. The collapsed state keeps the image canvas wide on smaller
	desktops; labels remain available to assistive technology and as native titles.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import { session } from '$lib/api/session.svelte';
	import brandMark from '$lib/assets/damrs-mark.png';
	import {
		CaretLeft,
		CaretRight,
		ClipboardText,
		Eye,
		Gear,
		HardDrives,
		Heart,
		Images,
		ListBullets,
		ListChecks,
		Palette,
		ShareNetwork,
		Moon,
		Plug,
		Sun,
		SquaresFour,
		Tag,
		TreeStructure
	} from 'phosphor-svelte';
	let collapsed = $state(false);
	let darkMode = $state(true);

	onMount(() => {
		const stored = localStorage.getItem('damrs-theme');
		if (stored === 'dark' || stored === 'light') {
			document.documentElement.dataset.theme = stored;
			darkMode = stored === 'dark';
			return;
		}

		darkMode = window.matchMedia('(prefers-color-scheme: dark)').matches;
	});

	function toggleTheme() {
		darkMode = !darkMode;
		const theme = darkMode ? 'dark' : 'light';
		document.documentElement.dataset.theme = theme;
		localStorage.setItem('damrs-theme', theme);
	}

	const GROUPS = [
		{
			label: 'Library',
			links: [
				{ href: resolve('/assets'), label: 'Assets', icon: Images },
				{ href: resolve('/favourites'), label: 'Favourites', icon: Heart },
				{ href: resolve('/watches'), label: 'Watching', icon: Eye },
				{ href: resolve('/collections'), label: 'Collections', icon: SquaresFour },
				{ href: resolve('/shares'), label: 'Shares', icon: ShareNetwork }
			]
		},
		{
			label: 'Workflow',
			links: [
				{ href: resolve('/orders'), label: 'Orders', icon: ClipboardText },
				{ href: resolve('/worklists'), label: 'Worklists', icon: ListBullets },
				{ href: resolve('/review'), label: 'Review', icon: ListChecks }
			]
		},
		{
			label: 'Governance',
			links: [
				{ href: resolve('/schema'), label: 'Schema', icon: TreeStructure },
				{ href: resolve('/vocabularies'), label: 'Vocabularies', icon: Tag },
				{ href: resolve('/style'), label: 'Style', icon: Palette }
			]
		},
		{
			label: 'System',
			links: [
				{ href: resolve('/webhooks'), label: 'Webhooks', icon: Plug },
				{ href: resolve('/storage'), label: 'Storage', icon: HardDrives },
				{ href: resolve('/settings'), label: 'Settings', icon: Gear }
			]
		}
	];

	function current(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(`${href}/`);
	}

	const portal = $derived(
		page.url.pathname.startsWith('/share/') || page.url.pathname.startsWith('/portal/')
	);
</script>

{#if !portal}
	<nav
		aria-label="Main"
		data-collapsed={collapsed}
		class="app-rail fixed inset-y-0 left-0 z-30 flex w-52 flex-col border-r border-line bg-bg px-3 py-4"
	>
		<a
			href={resolve('/')}
			class="brand-lockup mb-7 flex min-h-11 items-center gap-2.5 rounded-md px-1.5"
			aria-label="dam.rs home"
		>
			<img src={brandMark} alt="" class="brand-mark h-8 w-7 shrink-0 object-contain" />
			<span class="brand-copy min-w-0">
				<span class="block text-lg font-semibold tracking-[-0.035em] text-fg"
					>dam<span class="text-accent">.</span>rs</span
				>
				<span class="block truncate text-[10px] tracking-wide text-muted"
					>Find it. Trust it. Use it.</span
				>
			</span>
		</a>

		<div class="min-h-0 flex-1 space-y-5 overflow-x-hidden overflow-y-auto">
			{#each GROUPS as group (group.label)}
				<section aria-label={group.label}>
					<p
						class="rail-group mb-1.5 px-2 text-[10px] font-semibold tracking-[0.16em] text-muted uppercase"
					>
						{group.label}
					</p>
					<ul class="space-y-0.5">
						{#each group.links as link (link.href)}
							<li>
								<a
									href={link.href}
									aria-current={current(link.href) ? 'page' : undefined}
									title={collapsed ? link.label : undefined}
									class="rail-link flex h-9 items-center gap-2.5 rounded-md px-2 text-sm text-muted hover:bg-surface hover:text-fg aria-[current=page]:bg-surface aria-[current=page]:font-medium aria-[current=page]:text-accent"
								>
									<link.icon
										size={18}
										weight={current(link.href) ? 'fill' : 'regular'}
										aria-hidden="true"
									/>
									<span class="rail-label truncate">{link.label}</span>
								</a>
							</li>
						{/each}
					</ul>
				</section>
			{/each}
		</div>

		<div class="mt-4 border-t border-line pt-3">
			<button
				type="button"
				class="mb-1 flex h-9 w-full items-center gap-2.5 rounded-md px-2 text-xs text-muted hover:bg-surface hover:text-fg"
				onclick={toggleTheme}
				aria-label={darkMode ? 'Use light theme' : 'Use dark theme'}
				title={collapsed ? (darkMode ? 'Use light theme' : 'Use dark theme') : undefined}
			>
				{#if darkMode}
					<Sun size={17} aria-hidden="true" />
					<span class="rail-label">Light mode</span>
				{:else}
					<Moon size={17} aria-hidden="true" />
					<span class="rail-label">Dark mode</span>
				{/if}
			</button>

			<div class="connection flex min-h-8 items-center gap-2 px-2 text-[11px]">
				<span
					aria-hidden="true"
					class="h-1.5 w-1.5 shrink-0 rounded-full {session.connected
						? 'bg-state-rights-allowed'
						: 'bg-state-rights-denied'}"
				></span>
				{#if session.connected}
					<span class="rail-label truncate font-mono text-muted" title={session.visible}
						>{session.visible}</span
					>
				{:else}
					<a
						class="rail-label truncate text-state-rights-denied-fg underline"
						href={resolve('/settings')}>Not connected</a
					>
				{/if}
			</div>

			<button
				type="button"
				class="mt-2 flex h-9 w-full items-center gap-2.5 rounded-md border border-line px-2 text-xs text-muted hover:bg-surface hover:text-fg"
				onclick={() => (collapsed = !collapsed)}
				aria-label={collapsed ? 'Expand navigation' : 'Collapse navigation'}
				aria-expanded={!collapsed}
			>
				{#if collapsed}
					<CaretRight size={17} aria-hidden="true" />
				{:else}
					<CaretLeft size={17} aria-hidden="true" />
				{/if}
				<span class="rail-label">Collapse</span>
			</button>
		</div>
	</nav>
{/if}
