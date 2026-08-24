<!--
	The product rail: a small number of durable neighbourhoods rather than one flat list.

	The application had outgrown its horizontal header: twenty-one equal-weight destinations left less room
	for the asset work itself and made governance look as frequent as browsing. Worse, measured, the row
	wanted 1461px — so at 1024 and 1280, an iPad in landscape and the default MacBook Air, the last items sat
	outside the viewport entirely and were unreachable by mouse.

	Grouping preserves every route while making the operator's mental model visible. The collapsed state keeps
	the image canvas wide on smaller desktops; labels remain available to assistive technology and as native
	titles.

	**Every route belongs to exactly one group, and that is a property to keep.** The rail is the only way most
	of these screens are reachable, so a destination added to `src/routes` and not added here is a screen that
	exists and cannot be found. Seven of them arrived while this rail was being designed on a branch and had to
	be placed by hand at the merge; the placement comments below are what makes the next one obvious rather
	than a guess.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { isPublicRoute } from '$lib/chrome';
	import { resolve } from '$app/paths';
	import { branding } from '$lib/api/branding.svelte';
	import { session } from '$lib/api/session.svelte';
	import brandMark from '$lib/assets/damrs-mark.svg';
	import {
		Browsers,
		CaretLeft,
		CaretRight,
		ChartLine,
		ClipboardText,
		Copy,
		Eye,
		Gear,
		HardDrives,
		Heart,
		Images,
		PaintBrush,
		ShieldCheck,
		Stamp,
		Users,
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
		// Branding is loaded here because this is the one component on every route, and the store guards
		// against repeat calls — so mounting it per navigation costs nothing after the first. It rides along
		// with the theme read rather than in a second `onMount`, which would be two lifecycles doing one job.
		void branding.ensure();

		const stored = localStorage.getItem('damrs-theme');
		if (stored === 'dark' || stored === 'light') {
			document.documentElement.dataset.theme = stored;
			darkMode = stored === 'dark';
			return;
		}

		// No stored choice, so the attribute stays off and the palette's own `prefers-color-scheme` block
		// decides. Reading the query here only keeps the toggle's icon honest about what is on screen.
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
				// The two private lists sit next to Assets because that is what they are — the library
				// filtered to what this person marked, not separate features.
				{ href: resolve('/favourites'), label: 'Favourites', icon: Heart },
				{ href: resolve('/watches'), label: 'Watching', icon: Eye },
				// A collection is what a portal publishes and a portal is a share with a front door, so the
				// two belong side by side.
				{ href: resolve('/collections'), label: 'Collections', icon: SquaresFour },
				{ href: resolve('/shares'), label: 'Shares', icon: ShareNetwork },
				// Last in Library rather than in System: Insights is read-only and about the library, not
				// about how the library is configured. It is the screen that answers "is any of this being
				// used at all".
				{ href: resolve('/insights'), label: 'Insights', icon: ChartLine }
			]
		},
		{
			label: 'Workflow',
			links: [
				{ href: resolve('/orders'), label: 'Orders', icon: ClipboardText },
				{ href: resolve('/worklists'), label: 'Worklists', icon: ListBullets },
				{ href: resolve('/review'), label: 'Review', icon: ListChecks },
				// Beside the other two queues because it is the same kind of thing: a judgement the library
				// needs from a person, computed from what it already holds.
				{ href: resolve('/duplicates'), label: 'Duplicates', icon: Copy },
				// And with them for the same reason, with one difference worth stating: a worklist and a
				// duplicate queue are computed, while a proofing round is somebody having *asked*. A reviewer
				// opening the application to see what is waiting on them should find all four together.
				{ href: resolve('/proofing'), label: 'Proofing', icon: Stamp }
			]
		},
		{
			label: 'Governance',
			links: [
				{ href: resolve('/schema'), label: 'Schema', icon: TreeStructure },
				// The same kind of work as Schema: deciding what the library's own vocabulary means.
				{ href: resolve('/vocabularies'), label: 'Vocabularies', icon: Tag },
				{ href: resolve('/style'), label: 'Style', icon: Palette },
				// The hash-chained record. Here rather than in System because it is the screen an auditor is
				// pointed at, which is a different visit from an operator's.
				{ href: resolve('/governance'), label: 'Governance', icon: ShieldCheck },
				// Beside it because they are two halves of one job: who has access, and the record of who
				// gave it to them. Every change on this screen appears on that one.
				{ href: resolve('/people'), label: 'People', icon: Users }
			]
		},
		{
			label: 'System',
			links: [
				{ href: resolve('/webhooks'), label: 'Webhooks', icon: Plug },
				// Beside Webhooks because they are the two halves of another one: a webhook is how the
				// library tells another system something changed, and a connected site is that system.
				{ href: resolve('/connectors'), label: 'Sites', icon: Browsers },
				{ href: resolve('/storage'), label: 'Storage', icon: HardDrives },
				// Configuration rather than content, and the one screen where a tenant makes the application
				// look like theirs — so it sits against Settings.
				{ href: resolve('/branding'), label: 'Branding', icon: PaintBrush },
				{ href: resolve('/settings'), label: 'Settings', icon: Gear }
			]
		}
	];

	function current(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(`${href}/`);
	}

	// A portal has no nav: its visitor is an external recipient with no account, and an app chrome saying
	// "Not connected" invites them to try to connect to something that was never theirs. `isPublicRoute` is
	// shared with the layout, which drops the rail's margin on the same routes.
	const portal = $derived(isPublicRoute(page.url.pathname));
</script>

{#if !portal}
	<nav
		aria-label="Main"
		data-collapsed={collapsed}
		class="app-rail fixed inset-y-0 left-0 z-30 flex w-52 flex-col border-r border-line bg-bg px-3 py-4"
	>
		<!--
			The lockup is the *tenant's*, and falls back to ours.

			The rail was designed with `dam.rs` fixed in this corner, which is right for an unbranded
			deployment and wrong for a customer: `/branding` is a shipped feature whose whole point is that a
			tenant's library looks like theirs, and a vendor name pinned above every screen is the thing it
			exists to remove.

			So the mark slot has three states, and each of the three exists because of a bug the other two
			would cause:

			**A tenant logo, when there is one.** Theirs, not ours.

			**Otherwise the tenant's accent, as a bar.** Not our mark: substituting it here is what my first
			attempt at this merge did, and it silently deleted the only place `branding.accent` appears in the
			shell — a colour the customer chose, rendered nowhere. The bar is decorative and carries no text,
			so it cannot fail a contrast check, which is why the accent is safe here and not on the app's own
			`--color-accent` pairs; `layout.css` has the argument.

			**Our mark only when nobody is connected**, because then there is no tenant to be branded as. That
			is the state somebody reaches before they belong to anybody's library, and it is where the new
			brand identity belongs.

			**And nothing visible until the name is known.** The first version of the branded nav fell back to
			"damrs", which put a flash of the vendor's name into every page load of every customer's library.
			Driving the real page is what showed it: "immediately: damrs | settled: Acme Picture Library".

			**But a link needs discernible text, always.** Removing that fallback once left this link with *no*
			accessible name, because the mark beside it is `aria-hidden` — axe called it `link-name (serious)`
			on every screen in the application. A visual fix turned into a worse accessibility bug than the one
			it fixed. Hence the `sr-only` "Home" while the name is unresolved.
		-->
		<a
			href={resolve('/')}
			class="brand-lockup mb-7 flex min-h-11 items-center gap-2.5 rounded-md px-1.5"
		>
			{#if branding.logoUrl}
				<img
					src={branding.logoUrl}
					alt=""
					aria-hidden="true"
					class="brand-mark h-8 w-7 shrink-0 object-contain"
				/>
			{:else if session.connected}
				<span
					aria-hidden="true"
					class="brand-mark h-6 w-1.5 shrink-0 rounded-full"
					style="background-color: {branding.accent}"
				></span>
			{:else}
				<!-- Traced from the PNG it replaces; `docs/brand/README.md` has the measurements. -->
				<img src={brandMark} alt="" class="brand-mark h-8 w-7 shrink-0 object-contain" />
			{/if}
			<span class="brand-copy min-w-0">
				{#if branding.name}
					<span class="block truncate text-lg font-semibold tracking-[-0.035em] text-fg"
						>{branding.name}</span
					>
					<!--
						The tagline is ours, so it does not appear over somebody else's name. A customer's
						library saying "Find it. Trust it. Use it." is the vendor talking in their building.
					-->
				{:else if !session.connected}
					<span class="block text-lg font-semibold tracking-[-0.035em] text-fg"
						>dam<span class="text-accent">.</span>rs</span
					>
					<span class="block truncate text-[10px] tracking-wide text-muted"
						>Find it. Trust it. Use it.</span
					>
				{:else}
					<span class="sr-only">Home</span>
				{/if}
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
					<!-- The prefix only. It is what an audit log shows, so displaying it discloses nothing new. -->
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
