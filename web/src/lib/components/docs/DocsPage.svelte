<script lang="ts">
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import brandMark from '$lib/assets/damrs-mark.svg';
	import {
		ArrowLeft,
		ArrowRight,
		ArrowUpRight,
		BookOpen,
		Check,
		Copy,
		GithubLogo,
		List,
		MagnifyingGlass,
		WarningCircle
	} from 'phosphor-svelte';
	import { DOC_GROUPS, DOC_ORDER, DOCS, type DocSlug } from '$lib/docs/content';

	let { slug }: { slug: DocSlug } = $props();

	const doc = $derived(DOCS[slug]);
	const canonicalUrl = $derived(
		slug === 'getting-started'
			? 'https://damrs.github.io/tour/docs/'
			: `https://damrs.github.io/tour/docs/${slug}/`
	);
	const index = $derived(DOC_ORDER.indexOf(slug));
	const previous = $derived(index > 0 ? DOCS[DOC_ORDER[index - 1]] : null);
	const next = $derived(index < DOC_ORDER.length - 1 ? DOCS[DOC_ORDER[index + 1]] : null);

	let query = $state('');
	let sidebarOpen = $state(false);
	let copied = $state('');
	let activeSection = $state('');
	let searchInput: HTMLInputElement;

	const matching = $derived.by(() => {
		const needle = query.trim().toLowerCase();
		if (!needle) return new Set(DOC_ORDER);

		return new Set(
			DOC_ORDER.filter((candidate) => {
				const page = DOCS[candidate];
				return [page.nav, page.title, page.description, ...page.keywords]
					.join(' ')
					.toLowerCase()
					.includes(needle);
			})
		);
	});

	const hasMatches = $derived(matching.size > 0);

	onMount(() => {
		activeSection = doc.sections[0]?.id ?? '';
		const headings = Array.from(document.querySelectorAll<HTMLElement>('[data-doc-heading]'));
		const observer = new IntersectionObserver(
			(entries) => {
				const visible = entries
					.filter((entry) => entry.isIntersecting)
					.sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
				if (visible?.target.id) activeSection = visible.target.id;
			},
			{ rootMargin: '-18% 0px -72%', threshold: [0, 0.15, 0.5] }
		);

		headings.forEach((heading) => observer.observe(heading));

		function focusSearch(event: KeyboardEvent) {
			if (
				event.key === '/' &&
				document.activeElement?.tagName !== 'INPUT' &&
				document.activeElement?.tagName !== 'TEXTAREA'
			) {
				event.preventDefault();
				searchInput?.focus();
			}
		}

		window.addEventListener('keydown', focusSearch);
		return () => {
			observer.disconnect();
			window.removeEventListener('keydown', focusSearch);
		};
	});

	async function copyCode(id: string, code: string) {
		await navigator.clipboard.writeText(code);
		copied = id;
		window.setTimeout(() => {
			if (copied === id) copied = '';
		}, 1600);
	}

	function closeSidebar() {
		sidebarOpen = false;
	}
</script>

<svelte:head>
	<link rel="canonical" href={canonicalUrl} />
	<meta name="description" content={doc.description} />
	<meta property="og:type" content="article" />
	<meta property="og:site_name" content="dam.rs documentation" />
	<meta property="og:title" content={doc.title} />
	<meta property="og:description" content={doc.description} />
	<meta property="og:url" content={canonicalUrl} />
	<meta name="twitter:card" content="summary" />
</svelte:head>

<div class="docs-shell">
	<header class="docs-header">
		<div class="header-inner">
			<a class="docs-brand" href={resolve('/tour')} aria-label="dam.rs product tour">
				<img src={brandMark} alt="" />
				<strong>dam<span>.</span>rs</strong>
			</a>
			<span class="brand-divider" aria-hidden="true"></span>
			<a class="docs-word" href={resolve('/tour/docs')}>Documentation</a>

			<div class="header-actions">
				<a href={resolve('/tour')}><ArrowLeft size={15} aria-hidden="true" /> Product tour</a>
				<a href="https://github.com/skippednote/dam.rs" target="_blank">
					<GithubLogo size={17} aria-hidden="true" /><span>GitHub</span>
				</a>
			</div>

			<button
				class="sidebar-toggle"
				type="button"
				aria-label="Toggle documentation navigation"
				aria-expanded={sidebarOpen}
				onclick={() => (sidebarOpen = !sidebarOpen)}
			>
				<List size={20} aria-hidden="true" />
			</button>
		</div>
	</header>

	<div class="docs-layout">
		<aside class:open={sidebarOpen} class="docs-sidebar" aria-label="Documentation navigation">
			<div class="search-wrap">
				<MagnifyingGlass size={16} aria-hidden="true" />
				<input
					bind:this={searchInput}
					bind:value={query}
					type="search"
					placeholder="Search documentation"
					aria-label="Search documentation"
				/>
				<kbd>/</kbd>
			</div>

			<nav aria-label="Documentation pages">
				{#each DOC_GROUPS as group (group.label)}
					{@const visibleItems = group.items.filter((item) => matching.has(item))}
					{#if visibleItems.length > 0}
						<section
							aria-labelledby={`docs-group-${group.label.replaceAll(' ', '-').toLowerCase()}`}
						>
							<h2 id={`docs-group-${group.label.replaceAll(' ', '-').toLowerCase()}`}>
								{group.label}
							</h2>
							<ul>
								{#each visibleItems as item (item)}
									{@const itemDoc = DOCS[item]}
									<li>
										<a
											href={resolve(itemDoc.href)}
											aria-current={item === slug ? 'page' : undefined}
											onclick={closeSidebar}
										>
											<span>{itemDoc.nav}</span>
											{#if item === slug}<i aria-hidden="true"></i>{/if}
										</a>
									</li>
								{/each}
							</ul>
						</section>
					{/if}
				{/each}

				{#if !hasMatches}
					<div class="empty-search">
						<MagnifyingGlass size={20} aria-hidden="true" />
						<strong>No matching topic</strong>
						<p>Try “upload”, “security” or “deploy”.</p>
					</div>
				{/if}
			</nav>

			<div class="sidebar-status">
				<span aria-hidden="true"></span>
				<div><strong>Active development</strong><small>Docs follow the current source.</small></div>
			</div>
		</aside>

		{#if sidebarOpen}
			<button
				class="sidebar-scrim"
				type="button"
				aria-label="Close documentation navigation"
				onclick={closeSidebar}
			></button>
		{/if}

		<article class="doc-article">
			<header class="article-header">
				<div class="breadcrumbs">
					<a href={resolve('/tour/docs')}>Docs</a>
					<span>/</span>
					<span>{doc.category}</span>
				</div>
				<h1>{doc.title}</h1>
				<p>{doc.description}</p>
				<div class="article-meta">
					<span>{doc.readingTime} read</span>
					<span>Source-backed</span>
					<!-- The source is a fixed repository URL from the local docs catalogue. -->
					<!-- eslint-disable-next-line svelte/no-navigation-without-resolve -->
					<a href={doc.source.href} target="_blank">
						{doc.source.label}<ArrowUpRight size={13} aria-hidden="true" />
					</a>
				</div>
			</header>

			<div class="mobile-toc">
				<span>On this page</span>
				<div>
					{#each doc.sections as section (section.id)}
						<a href={`#${section.id}`}>{section.title}</a>
					{/each}
				</div>
			</div>

			<div class="article-body">
				{#each doc.sections as section (section.id)}
					<section class="doc-section" aria-labelledby={section.id}>
						<h2 id={section.id} data-doc-heading>{section.title}</h2>
						{#if section.lede}<p class="section-lede">{section.lede}</p>{/if}

						{#each section.blocks as block, blockIndex (`${section.id}-${blockIndex}`)}
							{#if block.kind === 'paragraphs'}
								<div class="prose-copy">
									{#each block.content as paragraph (paragraph)}
										<!-- Content is authored in the local typed docs catalogue, never supplied by a request. -->
										<!-- eslint-disable-next-line svelte/no-at-html-tags -->
										<p>{@html paragraph}</p>
									{/each}
								</div>
							{:else if block.kind === 'list'}
								<ul class="doc-list">
									{#each block.items as item (item)}
										<li>
											<Check size={15} weight="bold" aria-hidden="true" />
											<!-- Content is authored in the local typed docs catalogue, never supplied by a request. -->
											<!-- eslint-disable-next-line svelte/no-at-html-tags -->
											<span>{@html item}</span>
										</li>
									{/each}
								</ul>
							{:else if block.kind === 'steps'}
								<ol class="doc-steps">
									{#each block.items as item, stepIndex (item.title)}
										<li>
											<span>0{stepIndex + 1}</span>
											<div>
												<h3>{item.title}</h3>
												<p>{item.body}</p>
											</div>
										</li>
									{/each}
								</ol>
							{:else if block.kind === 'code'}
								<div class="doc-code">
									<div class="code-heading">
										<span>{block.title}</span>
										<em>{block.language}</em>
										<button
											type="button"
											onclick={() => copyCode(`${section.id}-${blockIndex}`, block.code)}
										>
											{#if copied === `${section.id}-${blockIndex}`}
												<Check size={14} aria-hidden="true" /> Copied
											{:else}
												<Copy size={14} aria-hidden="true" /> Copy
											{/if}
										</button>
									</div>
									<pre><code>{block.code}</code></pre>
								</div>
							{:else if block.kind === 'callout'}
								<aside class="callout {block.tone}">
									<div>
										{#if block.tone === 'warning'}
											<WarningCircle size={19} weight="fill" aria-hidden="true" />
										{:else}
											<BookOpen size={19} weight="duotone" aria-hidden="true" />
										{/if}
									</div>
									<section>
										<h3>{block.title}</h3>
										<p>{block.body}</p>
									</section>
								</aside>
							{:else if block.kind === 'table'}
								<div class="table-wrap">
									<table>
										<thead>
											<tr>
												{#each block.headers as header (header)}<th scope="col">{header}</th>{/each}
											</tr>
										</thead>
										<tbody>
											{#each block.rows as row, rowIndex (rowIndex)}
												<tr>
													{#each row as cell, cellIndex (cellIndex)}
														<!-- Content is authored in the local typed docs catalogue, never supplied by a request. -->
														<!-- eslint-disable-next-line svelte/no-at-html-tags -->
														<td>{@html cell}</td>
													{/each}
												</tr>
											{/each}
										</tbody>
									</table>
								</div>
							{/if}
						{/each}
					</section>
				{/each}
			</div>

			<nav class="page-nav" aria-label="Adjacent documentation pages">
				{#if previous}
					<a class="previous" href={resolve(previous.href)}>
						<ArrowLeft size={17} aria-hidden="true" />
						<span><small>Previous</small><strong>{previous.nav}</strong></span>
					</a>
				{:else}
					<span></span>
				{/if}
				{#if next}
					<a class="next" href={resolve(next.href)}>
						<span><small>Next</small><strong>{next.nav}</strong></span>
						<ArrowRight size={17} aria-hidden="true" />
					</a>
				{/if}
			</nav>

			<footer class="docs-footer">
				<span>dam<span>.</span>rs documentation</span>
				<p>Find it. Trust it. Use it.</p>
			</footer>
		</article>

		<aside class="toc" aria-label="On this page">
			<h2>On this page</h2>
			<nav>
				{#each doc.sections as section (section.id)}
					<a
						href={`#${section.id}`}
						aria-current={activeSection === section.id ? 'location' : undefined}>{section.title}</a
					>
				{/each}
			</nav>
			<!-- The source is a fixed repository URL from the local docs catalogue. -->
			<!-- eslint-disable-next-line svelte/no-navigation-without-resolve -->
			<a class="source-link" href={doc.source.href} target="_blank">
				Edit this source <ArrowUpRight size={13} aria-hidden="true" />
			</a>
		</aside>
	</div>
</div>

<style>
	:global(html) {
		scroll-behavior: smooth;
		scroll-padding-top: 6.5rem;
	}

	:global(body) {
		margin: 0;
	}

	.docs-shell {
		--docs-bg: #090b10;
		--docs-surface: #10141c;
		--docs-raised: #171c27;
		--docs-line: rgba(151, 166, 196, 0.16);
		--docs-fg: #f3f5f9;
		--docs-muted: #98a4b8;
		--docs-accent: #7087ff;
		--docs-green: #69d69b;
		--docs-amber: #f2b760;
		min-height: 100vh;
		background: var(--docs-bg);
		color: var(--docs-fg);
		font-family:
			Inter,
			ui-sans-serif,
			system-ui,
			-apple-system,
			BlinkMacSystemFont,
			'Segoe UI',
			sans-serif;
	}

	.docs-shell * {
		box-sizing: border-box;
	}

	.docs-shell a {
		color: inherit;
		text-decoration: none;
	}

	.docs-header {
		position: sticky;
		top: 0;
		z-index: 50;
		border-bottom: 1px solid var(--docs-line);
		background: rgba(9, 11, 16, 0.86);
		backdrop-filter: blur(18px);
	}

	.header-inner {
		display: flex;
		align-items: center;
		width: min(1440px, 100%);
		height: 4.35rem;
		padding: 0 2rem;
		margin: 0 auto;
	}

	.docs-brand {
		display: inline-flex;
		align-items: center;
		gap: 0.55rem;
	}

	.docs-brand img {
		width: 1.2rem;
		height: 1.5rem;
	}

	.docs-brand strong {
		font-size: 1rem;
		font-weight: 720;
		letter-spacing: -0.05em;
	}

	.docs-brand strong span,
	.docs-footer > span span {
		color: var(--docs-accent);
	}

	.brand-divider {
		width: 1px;
		height: 1.4rem;
		margin: 0 0.9rem;
		background: var(--docs-line);
	}

	.docs-word {
		color: #bbc4d3 !important;
		font-size: 0.78rem;
		font-weight: 570;
	}

	.header-actions {
		display: flex;
		align-items: center;
		gap: 0.25rem;
		margin-left: auto;
	}

	.header-actions a {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		padding: 0.55rem 0.7rem;
		border-radius: 0.45rem;
		color: var(--docs-muted);
		font-size: 0.7rem;
		font-weight: 590;
	}

	.header-actions a:hover {
		background: var(--docs-raised);
		color: var(--docs-fg);
	}

	.sidebar-toggle {
		display: none;
	}

	.docs-layout {
		display: grid;
		grid-template-columns: 17rem minmax(0, 1fr) 13rem;
		width: min(1440px, 100%);
		min-height: calc(100vh - 4.35rem);
		margin: 0 auto;
	}

	.docs-sidebar {
		position: sticky;
		top: 4.35rem;
		display: flex;
		height: calc(100vh - 4.35rem);
		padding: 1.4rem 1.25rem 1.25rem 2rem;
		border-right: 1px solid var(--docs-line);
		flex-direction: column;
	}

	.search-wrap {
		position: relative;
		display: flex;
		align-items: center;
		margin-bottom: 1.5rem;
		color: #707c91;
	}

	.search-wrap > :global(svg) {
		position: absolute;
		left: 0.7rem;
		pointer-events: none;
	}

	.search-wrap input {
		width: 100%;
		height: 2.35rem;
		padding: 0 2rem 0 2.15rem;
		border: 1px solid var(--docs-line);
		border-radius: 0.55rem;
		outline: none;
		background: rgba(255, 255, 255, 0.025);
		color: var(--docs-fg);
		font: inherit;
		font-size: 0.68rem;
	}

	.search-wrap input:focus {
		border-color: rgba(112, 135, 255, 0.65);
		box-shadow: 0 0 0 3px rgba(112, 135, 255, 0.1);
	}

	.search-wrap input::placeholder {
		color: #727d91;
	}

	.search-wrap kbd {
		position: absolute;
		right: 0.55rem;
		display: grid;
		width: 1.2rem;
		height: 1.2rem;
		place-items: center;
		border: 1px solid var(--docs-line);
		border-radius: 0.25rem;
		background: var(--docs-raised);
		color: #838da0;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.54rem;
	}

	.docs-sidebar > nav {
		min-height: 0;
		overflow-y: auto;
	}

	.docs-sidebar section {
		margin-bottom: 1.5rem;
	}

	.docs-sidebar h2,
	.toc h2 {
		margin: 0 0 0.5rem;
		color: #7c879b;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.57rem;
		font-weight: 650;
		letter-spacing: 0.1em;
		text-transform: uppercase;
	}

	.docs-sidebar ul {
		padding: 0;
		margin: 0;
		list-style: none;
	}

	.docs-sidebar li {
		margin: 0.1rem 0;
	}

	.docs-sidebar li a {
		position: relative;
		display: flex;
		align-items: center;
		justify-content: space-between;
		min-height: 2rem;
		padding: 0.4rem 0.65rem;
		border-radius: 0.42rem;
		color: var(--docs-muted);
		font-size: 0.7rem;
		transition:
			background 140ms ease,
			color 140ms ease;
	}

	.docs-sidebar li a:hover {
		background: rgba(255, 255, 255, 0.035);
		color: var(--docs-fg);
	}

	.docs-sidebar li a[aria-current='page'] {
		background: rgba(112, 135, 255, 0.1);
		color: #b7c1ff;
		font-weight: 610;
	}

	.docs-sidebar li a i {
		width: 0.32rem;
		height: 0.32rem;
		border-radius: 50%;
		background: var(--docs-accent);
		box-shadow: 0 0 10px rgba(112, 135, 255, 0.8);
	}

	.empty-search {
		display: flex;
		padding: 2rem 0.5rem;
		flex-direction: column;
		align-items: center;
		color: #687388;
		text-align: center;
	}

	.empty-search strong {
		margin-top: 0.6rem;
		color: #a8b2c3;
		font-size: 0.72rem;
	}

	.empty-search p {
		margin: 0.3rem 0 0;
		font-size: 0.62rem;
	}

	.sidebar-status {
		display: flex;
		align-items: center;
		gap: 0.55rem;
		padding-top: 1rem;
		margin-top: auto;
		border-top: 1px solid var(--docs-line);
	}

	.sidebar-status > span {
		width: 0.42rem;
		height: 0.42rem;
		border-radius: 50%;
		background: var(--docs-amber);
		box-shadow: 0 0 8px rgba(242, 183, 96, 0.5);
	}

	.sidebar-status strong,
	.sidebar-status small {
		display: block;
	}

	.sidebar-status strong {
		font-size: 0.62rem;
		font-weight: 620;
	}

	.sidebar-status small {
		margin-top: 0.1rem;
		color: #7b879b;
		font-size: 0.55rem;
	}

	.sidebar-scrim {
		display: none;
	}

	.doc-article {
		width: min(100%, 52rem);
		padding: 5rem 4rem 3rem;
		margin: 0 auto;
	}

	.article-header {
		padding-bottom: 3.5rem;
		border-bottom: 1px solid var(--docs-line);
	}

	.breadcrumbs {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin-bottom: 1.25rem;
		color: #778397;
		font-size: 0.65rem;
	}

	.breadcrumbs a {
		color: #aeb9ff;
	}

	.breadcrumbs a:hover {
		text-decoration: underline;
	}

	.article-header h1 {
		max-width: 43rem;
		margin: 0;
		font-size: clamp(2.7rem, 5vw, 4.8rem);
		font-weight: 650;
		line-height: 0.98;
		letter-spacing: -0.065em;
	}

	.article-header > p {
		max-width: 41rem;
		margin: 1.35rem 0 0;
		color: #a3aec0;
		font-size: 1rem;
		line-height: 1.7;
	}

	.article-meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.6rem;
		margin-top: 1.6rem;
	}

	.article-meta > span,
	.article-meta > a {
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
		padding: 0.35rem 0.5rem;
		border: 1px solid var(--docs-line);
		border-radius: 999px;
		color: #8490a4;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.55rem;
	}

	.article-meta > a:hover {
		border-color: rgba(112, 135, 255, 0.45);
		color: #adbbff;
	}

	.mobile-toc {
		display: none;
	}

	.doc-section {
		padding-top: 4.5rem;
	}

	.doc-section h2 {
		margin: 0;
		font-size: 1.75rem;
		font-weight: 630;
		line-height: 1.2;
		letter-spacing: -0.035em;
	}

	.section-lede {
		margin: 0.75rem 0 0;
		color: #a8b3c5;
		font-size: 0.9rem;
		line-height: 1.65;
	}

	.prose-copy {
		margin-top: 1.4rem;
	}

	.prose-copy p {
		margin: 0 0 1rem;
		color: var(--docs-muted);
		font-size: 0.86rem;
		line-height: 1.78;
	}

	.prose-copy p :global(code),
	.doc-list :global(code) {
		padding: 0.14rem 0.3rem;
		border: 1px solid rgba(112, 135, 255, 0.17);
		border-radius: 0.28rem;
		background: rgba(112, 135, 255, 0.07);
		color: #b5c0ff;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.76em;
	}

	.prose-copy p :global(strong),
	.doc-list :global(strong) {
		color: #dce1eb;
		font-weight: 620;
	}

	.prose-copy p :global(a) {
		color: #aebaff;
		text-decoration: underline;
		text-decoration-color: rgba(174, 186, 255, 0.4);
		text-underline-offset: 0.18rem;
	}

	.doc-list {
		display: grid;
		gap: 0.8rem;
		padding: 0;
		margin: 1.5rem 0 0;
		list-style: none;
	}

	.doc-list li {
		display: grid;
		grid-template-columns: 1.4rem 1fr;
		align-items: start;
		color: var(--docs-muted);
		font-size: 0.82rem;
		line-height: 1.65;
	}

	.doc-list li > :global(svg) {
		margin-top: 0.3rem;
		color: var(--docs-accent);
	}

	.doc-steps {
		display: grid;
		gap: 0;
		padding: 0;
		margin: 1.6rem 0 0;
		border: 1px solid var(--docs-line);
		border-radius: 0.8rem;
		list-style: none;
		overflow: hidden;
	}

	.doc-steps li {
		display: grid;
		grid-template-columns: 2.5rem 1fr;
		gap: 1rem;
		padding: 1.15rem;
		border-bottom: 1px solid var(--docs-line);
		background: rgba(255, 255, 255, 0.012);
	}

	.doc-steps li:last-child {
		border-bottom: 0;
	}

	.doc-steps li > span {
		display: grid;
		width: 2.2rem;
		height: 2.2rem;
		place-items: center;
		border: 1px solid rgba(112, 135, 255, 0.25);
		border-radius: 0.55rem;
		background: rgba(112, 135, 255, 0.08);
		color: #9dadff;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.59rem;
	}

	.doc-steps h3 {
		margin: 0 0 0.3rem;
		font-size: 0.82rem;
		font-weight: 620;
	}

	.doc-steps p {
		margin: 0;
		color: var(--docs-muted);
		font-size: 0.73rem;
		line-height: 1.62;
	}

	.doc-code {
		overflow: hidden;
		margin-top: 1.5rem;
		border: 1px solid var(--docs-line);
		border-radius: 0.75rem;
		background: #06080c;
		box-shadow: 0 18px 50px rgba(0, 0, 0, 0.2);
	}

	.code-heading {
		display: flex;
		align-items: center;
		height: 2.75rem;
		padding: 0 0.8rem;
		border-bottom: 1px solid var(--docs-line);
		background: #0d1016;
	}

	.code-heading span {
		font-size: 0.65rem;
		font-weight: 600;
	}

	.code-heading em {
		margin-left: 0.5rem;
		color: #7c889d;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.53rem;
		font-style: normal;
	}

	.code-heading button {
		display: inline-flex;
		align-items: center;
		gap: 0.35rem;
		padding: 0.35rem 0.5rem;
		margin-left: auto;
		border: 0;
		border-radius: 0.35rem;
		background: rgba(255, 255, 255, 0.045);
		color: #8f9bad;
		font: inherit;
		font-size: 0.58rem;
		cursor: pointer;
	}

	.code-heading button:hover {
		background: rgba(112, 135, 255, 0.12);
		color: #b7c1ff;
	}

	.doc-code pre {
		overflow-x: auto;
		padding: 1.2rem;
		margin: 0;
	}

	.doc-code code {
		color: #c0c8d6;
		font-family: ui-monospace, 'SFMono-Regular', Consolas, monospace;
		font-size: 0.68rem;
		line-height: 1.8;
	}

	.callout {
		display: grid;
		grid-template-columns: 2.4rem 1fr;
		gap: 0.8rem;
		padding: 1rem;
		margin-top: 1.5rem;
		border: 1px solid rgba(112, 135, 255, 0.18);
		border-radius: 0.7rem;
		background: rgba(112, 135, 255, 0.06);
	}

	.callout > div {
		display: grid;
		width: 2.2rem;
		height: 2.2rem;
		place-items: center;
		border-radius: 0.5rem;
		background: rgba(112, 135, 255, 0.11);
		color: #9dacff;
	}

	.callout.warning {
		border-color: rgba(242, 183, 96, 0.2);
		background: rgba(242, 183, 96, 0.055);
	}

	.callout.warning > div {
		background: rgba(242, 183, 96, 0.1);
		color: var(--docs-amber);
	}

	.callout.success {
		border-color: rgba(105, 214, 155, 0.2);
		background: rgba(105, 214, 155, 0.05);
	}

	.callout.success > div {
		background: rgba(105, 214, 155, 0.1);
		color: var(--docs-green);
	}

	.callout section h3 {
		margin: 0.1rem 0 0.25rem;
		font-size: 0.75rem;
		font-weight: 650;
	}

	.callout section p {
		margin: 0;
		color: #9da8ba;
		font-size: 0.7rem;
		line-height: 1.6;
	}

	.table-wrap {
		overflow-x: auto;
		margin-top: 1.5rem;
		border: 1px solid var(--docs-line);
		border-radius: 0.75rem;
	}

	.table-wrap table {
		width: 100%;
		border-collapse: collapse;
		font-size: 0.68rem;
		text-align: left;
	}

	.table-wrap th,
	.table-wrap td {
		padding: 0.8rem 0.9rem;
		border-right: 1px solid var(--docs-line);
		border-bottom: 1px solid var(--docs-line);
		vertical-align: top;
	}

	.table-wrap th:last-child,
	.table-wrap td:last-child {
		border-right: 0;
	}

	.table-wrap tbody tr:last-child td {
		border-bottom: 0;
	}

	.table-wrap th {
		background: #0f131b;
		color: #c7cfdd;
		font-weight: 610;
	}

	.table-wrap td {
		background: rgba(255, 255, 255, 0.012);
		color: #929daf;
		line-height: 1.5;
	}

	.table-wrap td:first-child {
		color: #bbc4d3;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.62rem;
	}

	.page-nav {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 1rem;
		padding-top: 4rem;
		margin-top: 5rem;
		border-top: 1px solid var(--docs-line);
	}

	.page-nav a {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		min-height: 4.7rem;
		padding: 0.9rem 1rem;
		border: 1px solid var(--docs-line);
		border-radius: 0.65rem;
		background: rgba(255, 255, 255, 0.012);
		color: #8490a4;
		transition:
			border-color 140ms ease,
			background 140ms ease;
	}

	.page-nav a:hover {
		border-color: rgba(112, 135, 255, 0.4);
		background: rgba(112, 135, 255, 0.055);
	}

	.page-nav .next {
		justify-content: flex-end;
		text-align: right;
	}

	.page-nav small,
	.page-nav strong {
		display: block;
	}

	.page-nav small {
		margin-bottom: 0.22rem;
		color: #7b879b;
		font-size: 0.55rem;
	}

	.page-nav strong {
		color: #c5cdda;
		font-size: 0.7rem;
		font-weight: 610;
	}

	.docs-footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding-top: 2rem;
		margin-top: 3rem;
		border-top: 1px solid var(--docs-line);
		color: #7b879c;
		font-size: 0.58rem;
	}

	.docs-footer > span {
		font-weight: 660;
		letter-spacing: -0.02em;
	}

	.docs-footer p {
		margin: 0;
	}

	.toc {
		position: sticky;
		top: 4.35rem;
		height: calc(100vh - 4.35rem);
		padding: 3rem 1.5rem 1.5rem;
		border-left: 1px solid var(--docs-line);
	}

	.toc nav {
		display: grid;
		gap: 0.15rem;
	}

	.toc nav a {
		position: relative;
		padding: 0.35rem 0 0.35rem 0.65rem;
		border-left: 1px solid var(--docs-line);
		color: #778297;
		font-size: 0.62rem;
		line-height: 1.4;
		transition:
			border-color 140ms ease,
			color 140ms ease;
	}

	.toc nav a:hover,
	.toc nav a[aria-current='location'] {
		border-color: var(--docs-accent);
		color: #bac4ff;
	}

	.source-link {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		margin-top: 1.5rem;
		color: #778397 !important;
		font-size: 0.59rem;
	}

	.source-link:hover {
		color: #adb9ff !important;
	}

	@media (max-width: 1120px) {
		.docs-layout {
			grid-template-columns: 16rem minmax(0, 1fr);
		}

		.toc {
			display: none;
		}

		.mobile-toc {
			display: block;
			padding: 1rem;
			margin-top: 2rem;
			border: 1px solid var(--docs-line);
			border-radius: 0.65rem;
			background: rgba(255, 255, 255, 0.015);
		}

		.mobile-toc > span {
			display: block;
			margin-bottom: 0.55rem;
			color: #6f7a8e;
			font-family: ui-monospace, 'SFMono-Regular', monospace;
			font-size: 0.55rem;
			text-transform: uppercase;
			letter-spacing: 0.08em;
		}

		.mobile-toc > div {
			display: flex;
			flex-wrap: wrap;
			gap: 0.45rem 0.8rem;
		}

		.mobile-toc a {
			color: #a8b5ff;
			font-size: 0.62rem;
		}
	}

	@media (max-width: 760px) {
		.header-inner {
			padding: 0 1rem;
		}

		.brand-divider,
		.docs-word,
		.header-actions {
			display: none;
		}

		.sidebar-toggle {
			display: grid;
			width: 2.5rem;
			height: 2.5rem;
			padding: 0;
			margin-left: auto;
			place-items: center;
			border: 1px solid var(--docs-line);
			border-radius: 0.55rem;
			background: transparent;
			color: var(--docs-fg);
		}

		.docs-layout {
			display: block;
		}

		.docs-sidebar {
			position: fixed;
			top: 4.35rem;
			bottom: 0;
			left: 0;
			z-index: 40;
			width: min(19rem, calc(100% - 3rem));
			height: auto;
			padding: 1.25rem;
			background: #0b0e14;
			box-shadow: 20px 0 60px rgba(0, 0, 0, 0.55);
			transform: translateX(-105%);
			transition: transform 180ms ease;
		}

		.docs-sidebar.open {
			transform: translateX(0);
		}

		.sidebar-scrim {
			position: fixed;
			inset: 4.35rem 0 0;
			z-index: 30;
			display: block;
			border: 0;
			background: rgba(0, 0, 0, 0.58);
		}

		.doc-article {
			width: 100%;
			padding: 3.5rem 1.25rem 2rem;
		}

		.article-header h1 {
			font-size: 2.8rem;
		}

		.article-header > p {
			font-size: 0.9rem;
		}

		.doc-section {
			padding-top: 3.5rem;
		}

		.doc-section h2 {
			font-size: 1.5rem;
		}

		.page-nav {
			grid-template-columns: 1fr;
		}

		.page-nav > span {
			display: none;
		}
	}

	@media (max-width: 480px) {
		.article-meta > span:nth-child(2) {
			display: none;
		}

		.doc-steps li {
			grid-template-columns: 2rem 1fr;
			gap: 0.75rem;
		}

		.doc-steps li > span {
			width: 1.9rem;
			height: 1.9rem;
		}

		.callout {
			grid-template-columns: 1fr;
		}

		.table-wrap {
			margin-right: -1.25rem;
			border-right: 0;
			border-radius: 0.75rem 0 0 0.75rem;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		:global(html) {
			scroll-behavior: auto;
		}

		.docs-sidebar {
			transition: none;
		}
	}
</style>
