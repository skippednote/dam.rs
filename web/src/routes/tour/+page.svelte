<script lang="ts">
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import brandMark from '$lib/assets/damrs-mark.svg';
	import type { DocHref } from '$lib/docs/content';
	import interfacePreview from '../../../../docs/brand/signal-ledger-reference.jpg?url';
	import {
		ArrowRight,
		ArrowUpRight,
		BookOpen,
		CheckCircle,
		CloudArrowUp,
		Code,
		Database,
		Fingerprint,
		GithubLogo,
		HardDrives,
		Lightning,
		LockKey,
		MagnifyingGlass,
		ShieldCheck,
		Terminal,
		WarningCircle,
		XCircle
	} from 'phosphor-svelte';

	type Purpose = 'preview' | 'distribution';
	type Rights = 'cleared' | 'unknown' | 'expired';
	type ProductMode = 'find' | 'trust' | 'use' | 'operate';
	type DocTab = 'start' | 'architecture' | 'api' | 'operate';

	let activeSection = $state('overview');
	let purpose = $state<Purpose>('distribution');
	let rights = $state<Rights>('unknown');
	let productMode = $state<ProductMode>('find');
	let docTab = $state<DocTab>('start');
	let copied = $state('');
	let mobileMenuOpen = $state(false);

	const decision = $derived.by(() => {
		if (purpose === 'preview') {
			return {
				allowed: true,
				label: 'Preview delivered',
				detail: 'Internal previews remain visible while paperwork is still being assembled.',
				audit: 'purpose=internal_preview · rights verdict skipped by policy'
			};
		}

		if (rights === 'cleared') {
			return {
				allowed: true,
				label: 'Distribution allowed',
				detail: 'The signed request and current licence agree at the moment bytes leave.',
				audit: 'purpose=distribution · rights=cleared · decision=allow'
			};
		}

		return {
			allowed: false,
			label: 'Distribution refused',
			detail:
				rights === 'expired'
					? 'The asset stays findable, but an expired licence cannot release the original.'
					: 'Unknown never becomes permission. Add evidence, then try the same URL again.',
			audit: `purpose=distribution · rights=${rights} · decision=deny`
		};
	});

	const productModes: Array<{
		id: ProductMode;
		verb: string;
		title: string;
		copy: string;
		points: string[];
		crop: string;
	}> = [
		{
			id: 'find',
			verb: 'Find it',
			title: 'A library that explains every result.',
			copy: 'Search, facets, saved views and near-duplicate detection work over the same permission-scoped catalogue.',
			points: [
				'Explainable query language',
				'Hot previews for archived masters',
				'Perceptual similarity'
			],
			crop: '50% 42%'
		},
		{
			id: 'trust',
			verb: 'Trust it',
			title: 'Evidence travels with the asset.',
			copy: 'Rights, provenance, legal holds and model-assisted metadata stay separate, visible and auditable.',
			points: [
				'C2PA-aware provenance',
				'Hash-chained governance record',
				'Human review for assisted metadata'
			],
			crop: '92% 35%'
		},
		{
			id: 'use',
			verb: 'Use it',
			title: 'Distribution is a live decision.',
			copy: 'A signed URL permits an attempt. The delivery edge checks the intended use and current rights before releasing bytes.',
			points: [
				'Purpose-bound signed URLs',
				'Revocation without chasing copies',
				'Audited download decisions'
			],
			crop: '87% 73%'
		},
		{
			id: 'operate',
			verb: 'Operate it',
			title: 'Cold storage without a cold library.',
			copy: 'Originals can tier to archive while metadata, search, previews and future model inputs remain immediately available.',
			points: [
				'S3-compatible storage pools',
				'Async restore workflow',
				'Backup and restore drills'
			],
			crop: '8% 80%'
		}
	];

	const docTabs: Array<{
		id: DocTab;
		label: string;
		title: string;
		copy: string;
		command: string;
		guide: DocHref;
		links: Array<{ label: string; href: string }>;
	}> = [
		{
			id: 'start',
			label: 'Start',
			title: 'Run the full stack locally',
			copy: 'mise pins the toolchain. Docker supplies PostgreSQL and SeaweedFS; the API, worker and web app remain separate processes.',
			command: 'mise install\nmise run up\nmise run dev:seed',
			guide: '/tour/docs',
			links: [
				{
					label: 'Read the README',
					href: 'https://github.com/skippednote/dam.rs#local-development'
				},
				{
					label: 'Frontend guide',
					href: 'https://github.com/skippednote/dam.rs/blob/main/web/README.md'
				}
			]
		},
		{
			id: 'architecture',
			label: 'Architecture',
			title: 'Understand the invariants first',
			copy: 'Start with the delivery boundary, schema-per-tenant isolation and the rule that search never touches the original blob.',
			command:
				'damd  →  PostgreSQL + object storage\n  └── durable jobs  →  dam-worker\n      ├── verify + derive\n      └── index + enrich',
			guide: '/tour/docs/architecture',
			links: [
				{
					label: 'Architecture',
					href: 'https://github.com/skippednote/dam.rs/blob/main/ARCHITECTURE.md'
				},
				{
					label: 'Decision log',
					href: 'https://github.com/skippednote/dam.rs/blob/main/DECISIONS.md'
				}
			]
		},
		{
			id: 'api',
			label: 'API',
			title: 'One contract, generated clients',
			copy: 'The checked-in OpenAPI document describes 143 paths and 190 operations. Frontend types are generated from that same contract.',
			command:
				'mise run openapi\ncd web && pnpm run gen:api\n# contract drift fails the test suite',
			guide: '/tour/docs/api-mcp',
			links: [
				{
					label: 'OpenAPI JSON',
					href: 'https://github.com/skippednote/dam.rs/blob/main/openapi.json'
				},
				{
					label: 'MCP surface',
					href: 'https://github.com/skippednote/dam.rs/tree/main/crates/dam-mcp'
				}
			]
		},
		{
			id: 'operate',
			label: 'Operate',
			title: 'Deploy as an accountable system',
			copy: 'The backend image contains the API, worker and CLI. Production still needs TLS, durable search storage, authentication and a tested backup policy.',
			command:
				'mise run check:all\ndocker compose -f docker/compose.dev.yml up\n# read DEPLOY.md before rollout',
			guide: '/tour/docs/operations',
			links: [
				{
					label: 'Deployment guide',
					href: 'https://github.com/skippednote/dam.rs/blob/main/docker/DEPLOY.md'
				},
				{ label: 'Security', href: 'https://github.com/skippednote/dam.rs/blob/main/SECURITY.md' }
			]
		}
	];

	const currentProduct = $derived(productModes.find((mode) => mode.id === productMode)!);
	const currentDoc = $derived(docTabs.find((tab) => tab.id === docTab)!);

	onMount(() => {
		const sections = Array.from(document.querySelectorAll<HTMLElement>('section[id]'));
		const observer = new IntersectionObserver(
			(entries) => {
				const visible = entries
					.filter((entry) => entry.isIntersecting)
					.sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
				if (visible?.target.id) activeSection = visible.target.id;
			},
			{ rootMargin: '-24% 0px -62%', threshold: [0, 0.2, 0.5] }
		);

		sections.forEach((section) => observer.observe(section));
		return () => observer.disconnect();
	});

	async function copyCommand(command: string) {
		await navigator.clipboard.writeText(command);
		copied = docTab;
		window.setTimeout(() => {
			if (copied === docTab) copied = '';
		}, 1600);
	}
</script>

<svelte:head>
	<link rel="canonical" href="https://damrs.github.io/" />
	<meta
		name="description"
		content="Explore dam.rs: rights-aware digital asset management with enforceable delivery, auditable provenance and archive-ready storage."
	/>
	<meta property="og:type" content="website" />
	<meta property="og:site_name" content="dam.rs" />
	<meta property="og:title" content="dam.rs · Rights-aware digital asset management" />
	<meta
		property="og:description"
		content="Find every asset. Prove you can use it. Search, rights, provenance, storage and delivery in one auditable workflow."
	/>
	<meta property="og:url" content="https://damrs.github.io/" />
	<meta name="twitter:card" content="summary" />
</svelte:head>

<div class="tour-shell">
	<div class="ambient ambient-one" aria-hidden="true"></div>
	<div class="ambient ambient-two" aria-hidden="true"></div>

	<header class="site-header">
		<a class="brand" href="#overview" aria-label="dam.rs product tour">
			<img src={brandMark} alt="" />
			<span>dam<span>.</span>rs</span>
		</a>

		<button
			class="menu-button"
			type="button"
			aria-label="Toggle navigation"
			aria-expanded={mobileMenuOpen}
			onclick={() => (mobileMenuOpen = !mobileMenuOpen)}
		>
			<span></span><span></span>
		</button>

		<nav aria-label="Product tour" class:open={mobileMenuOpen}>
			{#each [['overview', 'Overview'], ['principle', 'Rights gate'], ['product', 'Product'], ['architecture', 'Architecture'], ['docs', 'Docs']] as item (item[0])}
				<a
					href={`#${item[0]}`}
					aria-current={activeSection === item[0] ? 'location' : undefined}
					onclick={() => (mobileMenuOpen = false)}>{item[1]}</a
				>
			{/each}
		</nav>

		<a class="github-link" href="https://github.com/skippednote/dam.rs" target="_blank">
			<GithubLogo size={18} aria-hidden="true" />
			<span>GitHub</span>
			<ArrowUpRight size={14} aria-hidden="true" />
		</a>
	</header>

	<div class="tour-main">
		<section id="overview" class="hero" aria-labelledby="hero-title">
			<div class="hero-grid" aria-hidden="true"></div>
			<div class="hero-copy">
				<div class="eyebrow"><span></span> Rights-aware digital asset management</div>
				<h1 id="hero-title">
					Find every asset.<br />
					<span>Prove you can use it.</span>
				</h1>
				<p class="hero-lede">
					Search, rights, provenance, storage and delivery in one auditable workflow—built in Rust
					for teams that cannot afford to guess.
				</p>
				<div class="hero-actions">
					<a class="button button-primary" href="#principle">
						Explore the system <ArrowRight size={17} aria-hidden="true" />
					</a>
					<a class="button button-quiet" href={resolve('/tour/docs')}>
						<BookOpen size={17} aria-hidden="true" /> Read the docs
					</a>
				</div>
				<div class="release-note">
					<span class="release-pulse" aria-hidden="true"></span>
					<strong>Active development</strong>
					<span>Core workflows are implemented; launch gates remain explicit.</span>
				</div>
			</div>

			<div class="hero-visual">
				<div class="visual-orbit orbit-one" aria-hidden="true"></div>
				<div class="visual-orbit orbit-two" aria-hidden="true"></div>
				<figure class="product-window">
					<div class="window-bar">
						<span></span><span></span><span></span>
						<div class="window-address">library.dam.rs/assets</div>
						<div class="secure-pill"><LockKey size={11} aria-hidden="true" /> scoped</div>
					</div>
					<img
						src={interfacePreview}
						alt="The dam.rs asset library with a selected campaign asset"
					/>
					<button class="hotspot hotspot-search" type="button" aria-label="Explainable search">
						<span></span><em>Explainable search</em>
					</button>
					<button class="hotspot hotspot-rights" type="button" aria-label="Rights decision">
						<span></span><em>Rights decision</em>
					</button>
					<button class="hotspot hotspot-proof" type="button" aria-label="Provenance evidence">
						<span></span><em>Provenance evidence</em>
					</button>
				</figure>

				<div class="floating-card delivery-card">
					<div class="card-icon ok"><CheckCircle size={17} weight="fill" aria-hidden="true" /></div>
					<div><span>Delivery decision</span><strong>Allowed · 48 ms</strong></div>
				</div>
				<div class="floating-card provenance-card">
					<div class="card-icon"><Fingerprint size={17} aria-hidden="true" /></div>
					<div><span>Provenance</span><strong>Verified chain</strong></div>
				</div>
			</div>

			<div class="hero-facts" aria-label="Project facts">
				<div><strong>143</strong><span>API paths</span></div>
				<div><strong>190</strong><span>operations</span></div>
				<div><strong>12</strong><span>Rust crates</span></div>
				<div><strong>46</strong><span>migrations</span></div>
				<div><strong>28</strong><span>UI surfaces</span></div>
			</div>
		</section>

		<section id="principle" class="section principle-section" aria-labelledby="principle-title">
			<div class="section-heading split-heading">
				<div>
					<p class="kicker">The one idea</p>
					<h2 id="principle-title">Rights are enforced where bytes leave.</h2>
				</div>
				<p>
					A badge is context. A signed URL is permission to attempt. The real decision happens when
					the asset is fetched, against the latest evidence.
				</p>
			</div>

			<div class="gate-lab">
				<div class="gate-controls">
					<div class="control-group">
						<span class="control-label">01 · Intended use</span>
						<div class="segmented" aria-label="Intended use">
							<button
								type="button"
								class:active={purpose === 'preview'}
								aria-pressed={purpose === 'preview'}
								onclick={() => (purpose = 'preview')}>Internal preview</button
							>
							<button
								type="button"
								class:active={purpose === 'distribution'}
								aria-pressed={purpose === 'distribution'}
								onclick={() => (purpose = 'distribution')}>Distribution</button
							>
						</div>
					</div>

					<div class="control-group">
						<span class="control-label">02 · Current rights evidence</span>
						<div class="rights-options" aria-label="Rights evidence">
							<button
								type="button"
								class:active={rights === 'cleared'}
								aria-pressed={rights === 'cleared'}
								onclick={() => (rights = 'cleared')}
								><span class="dot cleared"></span>Cleared</button
							>
							<button
								type="button"
								class:active={rights === 'unknown'}
								aria-pressed={rights === 'unknown'}
								onclick={() => (rights = 'unknown')}
								><span class="dot unknown"></span>Unknown</button
							>
							<button
								type="button"
								class:active={rights === 'expired'}
								aria-pressed={rights === 'expired'}
								onclick={() => (rights = 'expired')}
								><span class="dot expired"></span>Expired</button
							>
						</div>
					</div>

					<div class="request-packet">
						<span>Signed request</span>
						<code>asset_74cc · {purpose} · exp 00:54</code>
						<div class="packet-line" aria-hidden="true"><i></i></div>
					</div>
				</div>

				<div class="gate-core" class:denied={!decision.allowed} aria-hidden="true">
					<div class="gate-ring ring-a"></div>
					<div class="gate-ring ring-b"></div>
					<div class="gate-mark">
						{#if decision.allowed}
							<ShieldCheck size={42} weight="duotone" />
						{:else}
							<LockKey size={42} weight="duotone" />
						{/if}
					</div>
					<span>DELIVERY GATE</span>
				</div>

				<div class="gate-result" class:denied={!decision.allowed} aria-live="polite">
					<div class="decision-badge">
						{#if decision.allowed}
							<CheckCircle size={19} weight="fill" aria-hidden="true" />
						{:else}
							<XCircle size={19} weight="fill" aria-hidden="true" />
						{/if}
						Decision now
					</div>
					<h3>{decision.label}</h3>
					<p>{decision.detail}</p>
					<code>{decision.audit}</code>
					<div class="result-footer">
						<span><Database size={15} aria-hidden="true" /> audit appended</span>
						<span>policy v18</span>
					</div>
				</div>
			</div>
		</section>

		<section id="product" class="section product-section" aria-labelledby="product-title">
			<div class="section-heading">
				<p class="kicker">One accountable workflow</p>
				<h2 id="product-title">A DAM is more than a grid of thumbnails.</h2>
				<p>
					Move through the product by the job it must do. The same asset stays explainable from
					ingest to publication.
				</p>
			</div>

			<div class="product-explorer">
				<div class="product-tabs" role="tablist" aria-label="Product capabilities">
					{#each productModes as mode, index (mode.id)}
						<button
							type="button"
							role="tab"
							aria-selected={productMode === mode.id}
							aria-controls={`product-panel-${mode.id}`}
							class:active={productMode === mode.id}
							onclick={() => (productMode = mode.id)}
						>
							<span>0{index + 1}</span>
							<strong>{mode.verb}</strong>
						</button>
					{/each}
				</div>

				<div
					class="product-panel"
					id={`product-panel-${currentProduct.id}`}
					role="tabpanel"
					tabindex="0"
				>
					<div class="product-copy">
						<p class="mode-label">{currentProduct.verb}</p>
						<h3>{currentProduct.title}</h3>
						<p>{currentProduct.copy}</p>
						<ul>
							{#each currentProduct.points as point (point)}
								<li><CheckCircle size={17} weight="fill" aria-hidden="true" />{point}</li>
							{/each}
						</ul>
					</div>
					<div class="product-crop">
						<img src={interfacePreview} alt="" style={`object-position: ${currentProduct.crop}`} />
						<div class="crop-label"><span></span> Product surface</div>
					</div>
				</div>
			</div>

			<div class="capability-strip">
				<div><MagnifyingGlass size={22} aria-hidden="true" /><span>Search</span></div>
				<div><Fingerprint size={22} aria-hidden="true" /><span>Provenance</span></div>
				<div><ShieldCheck size={22} aria-hidden="true" /><span>Rights</span></div>
				<div><HardDrives size={22} aria-hidden="true" /><span>Storage</span></div>
				<div><Lightning size={22} aria-hidden="true" /><span>Automation</span></div>
				<div><Code size={22} aria-hidden="true" /><span>Integrations</span></div>
			</div>
		</section>

		<section
			id="architecture"
			class="section architecture-section"
			aria-labelledby="architecture-title"
		>
			<div class="section-heading split-heading">
				<div>
					<p class="kicker">System map</p>
					<h2 id="architecture-title">The original may sleep. The library stays awake.</h2>
				</div>
				<p>
					Metadata, previews and search stay hot while masters move through lower-cost storage. A
					model upgrade never has to thaw the archive.
				</p>
			</div>

			<div class="architecture-map" aria-label="Asset ingest and storage flow">
				<div class="flow-line" aria-hidden="true"><span></span></div>
				<div class="arch-node node-ingest">
					<div class="node-icon"><CloudArrowUp size={24} aria-hidden="true" /></div>
					<span>01</span>
					<h3>Ingest</h3>
					<p>Stream, hash, sniff and scan.</p>
				</div>
				<div class="arch-node node-derive">
					<div class="node-icon"><Lightning size={24} aria-hidden="true" /></div>
					<span>02</span>
					<h3>Derive</h3>
					<p>Preview, proxy and evidence.</p>
				</div>
				<div class="arch-node node-index">
					<div class="node-icon"><MagnifyingGlass size={24} aria-hidden="true" /></div>
					<span>03</span>
					<h3>Index</h3>
					<p>Search only after previews exist.</p>
				</div>
				<div class="arch-node node-deliver">
					<div class="node-icon"><ShieldCheck size={24} aria-hidden="true" /></div>
					<span>04</span>
					<h3>Deliver</h3>
					<p>Evaluate rights, record, release.</p>
				</div>

				<div class="storage-rail">
					<div class="storage-title">
						<HardDrives size={18} aria-hidden="true" /> S3-compatible pools
					</div>
					<div class="tier hot">
						<span>HOT</span><strong>Previews + search substrate</strong><em>instant</em>
					</div>
					<div class="tier cool">
						<span>COOL</span><strong>Recent originals</strong><em>instant + fee</em>
					</div>
					<div class="tier archive">
						<span>ARCHIVE</span><strong>Older masters</strong><em>async restore</em>
					</div>
				</div>
			</div>

			<div class="architecture-notes">
				<article>
					<span>Isolation</span>
					<h3>One PostgreSQL schema per tenant</h3>
					<p>
						Requests set the search path inside a transaction; a missing tenant predicate cannot
						cross a boundary.
					</p>
				</article>
				<article>
					<span>Recovery</span>
					<h3>Search is a derived cache</h3>
					<p>
						PostgreSQL remains the record. Tantivy and vectors can be rebuilt, upgraded or replaced.
					</p>
				</article>
				<article>
					<span>Scale</span>
					<h3>Split by failure profile</h3>
					<p>
						The API stays latency-sensitive; the worker owns CPU-heavy and untrusted media
						processing.
					</p>
				</article>
			</div>
		</section>

		<section id="docs" class="section docs-section" aria-labelledby="docs-title">
			<div class="section-heading split-heading">
				<div>
					<p class="kicker">Documentation</p>
					<h2 id="docs-title">Read the reason, then run the command.</h2>
				</div>
				<p>
					The repository documents the contract, architectural decisions, deployment shape and
					unfinished work. Nothing is hidden behind a glossy status page.
				</p>
			</div>

			<div class="docs-console">
				<div class="docs-tabs" role="tablist" aria-label="Documentation topics">
					{#each docTabs as tab (tab.id)}
						<button
							type="button"
							role="tab"
							aria-selected={docTab === tab.id}
							class:active={docTab === tab.id}
							onclick={() => (docTab = tab.id)}>{tab.label}</button
						>
					{/each}
				</div>

				<div class="docs-panel" role="tabpanel" tabindex="0">
					<div class="docs-copy">
						<div class="doc-icon">
							{#if docTab === 'start'}
								<Terminal size={24} aria-hidden="true" />
							{:else if docTab === 'architecture'}
								<Database size={24} aria-hidden="true" />
							{:else if docTab === 'api'}
								<Code size={24} aria-hidden="true" />
							{:else}
								<HardDrives size={24} aria-hidden="true" />
							{/if}
						</div>
						<h3>{currentDoc.title}</h3>
						<p>{currentDoc.copy}</p>
						<div class="doc-links">
							<a class="full-guide" href={resolve(currentDoc.guide)}>
								Open full guide<ArrowRight size={14} aria-hidden="true" />
							</a>
							{#each currentDoc.links as link (link.href)}
								<!-- The values are fixed external GitHub URLs from `docTabs`, not app routes. -->
								<!-- eslint-disable-next-line svelte/no-navigation-without-resolve -->
								<a href={link.href} target="_blank"
									>{link.label}<ArrowUpRight size={14} aria-hidden="true" /></a
								>
							{/each}
						</div>
					</div>

					<div class="code-window">
						<div class="code-bar">
							<span><Terminal size={14} aria-hidden="true" /> terminal</span>
							<button type="button" onclick={() => copyCommand(currentDoc.command)}>
								{copied === docTab ? 'Copied' : 'Copy'}
							</button>
						</div>
						<pre><code>{currentDoc.command}</code></pre>
					</div>
				</div>
			</div>

			<div class="doc-cards">
				<a href="https://github.com/skippednote/dam.rs/blob/main/TASKS.md" target="_blank">
					<span>Live ledger</span>
					<h3>What is built, parked and next</h3>
					<ArrowUpRight size={18} aria-hidden="true" />
				</a>
				<a href="https://github.com/skippednote/dam.rs/blob/main/CONTRIBUTING.md" target="_blank">
					<span>Contribution</span>
					<h3>The bar every change must clear</h3>
					<ArrowUpRight size={18} aria-hidden="true" />
				</a>
				<a
					href="https://github.com/skippednote/dam.rs/blob/main/docs/brand/README.md"
					target="_blank"
				>
					<span>Identity</span>
					<h3>Brand, interface and voice</h3>
					<ArrowUpRight size={18} aria-hidden="true" />
				</a>
			</div>
		</section>

		<section class="closing" aria-labelledby="closing-title">
			<img src={brandMark} alt="" />
			<p>Find it. Trust it. Use it.</p>
			<h2 id="closing-title">The asset is only useful when the evidence travels with it.</h2>
			<div>
				<a
					class="button button-primary"
					href="https://github.com/skippednote/dam.rs"
					target="_blank"
				>
					View the source <GithubLogo size={17} aria-hidden="true" />
				</a>
				<a class="button button-quiet" href={resolve('/tour/docs')}
					>Get started <ArrowRight size={17} aria-hidden="true" /></a
				>
			</div>
		</section>
	</div>

	<footer>
		<a class="brand footer-brand" href="#overview"
			><img src={brandMark} alt="" /><span>dam<span>.</span>rs</span></a
		>
		<p>Rights-aware digital asset management. Apache-2.0.</p>
		<div>
			<span><WarningCircle size={14} aria-hidden="true" /> Active development</span>
			<a href="https://github.com/skippednote/dam.rs/blob/main/SECURITY.md" target="_blank"
				>Security</a
			>
		</div>
	</footer>
</div>

<style>
	:global(html) {
		scroll-behavior: smooth;
		scroll-padding-top: 6rem;
	}

	:global(body) {
		margin: 0;
	}

	.tour-shell {
		--tour-bg: #080a0f;
		--tour-surface: #0f131c;
		--tour-raised: #171d29;
		--tour-line: rgba(151, 166, 196, 0.16);
		--tour-fg: #f5f7fb;
		--tour-muted: #9ba6ba;
		--tour-accent: #6b83ff;
		--tour-accent-soft: rgba(107, 131, 255, 0.14);
		--tour-green: #66d59a;
		--tour-red: #ff796d;
		position: relative;
		min-height: 100vh;
		overflow: hidden;
		background: var(--tour-bg);
		color: var(--tour-fg);
		font-family:
			Inter,
			ui-sans-serif,
			system-ui,
			-apple-system,
			BlinkMacSystemFont,
			'Segoe UI',
			sans-serif;
	}

	.tour-shell * {
		box-sizing: border-box;
	}

	.tour-shell a {
		color: inherit;
		text-decoration: none;
	}

	.ambient {
		position: absolute;
		z-index: 0;
		width: 42rem;
		height: 42rem;
		border-radius: 50%;
		filter: blur(120px);
		pointer-events: none;
		opacity: 0.14;
	}

	.ambient-one {
		top: 8rem;
		right: -20rem;
		background: #3d5dff;
	}

	.ambient-two {
		top: 92rem;
		left: -24rem;
		background: #3855d9;
	}

	.site-header {
		position: fixed;
		top: 1rem;
		left: 50%;
		z-index: 50;
		display: grid;
		grid-template-columns: 1fr auto 1fr;
		align-items: center;
		width: min(1180px, calc(100% - 2rem));
		min-height: 3.75rem;
		padding: 0.5rem 0.6rem 0.5rem 1rem;
		border: 1px solid var(--tour-line);
		border-radius: 1rem;
		background: rgba(8, 10, 15, 0.78);
		box-shadow: 0 18px 60px rgba(0, 0, 0, 0.24);
		backdrop-filter: blur(20px);
		transform: translateX(-50%);
	}

	.brand {
		display: inline-flex;
		align-items: center;
		gap: 0.65rem;
		width: max-content;
		font-size: 1.1rem;
		font-weight: 720;
		letter-spacing: -0.05em;
	}

	.brand img {
		width: 1.35rem;
		height: 1.65rem;
	}

	.brand span span {
		color: var(--tour-accent);
	}

	.site-header nav {
		display: flex;
		align-items: center;
		gap: 0.15rem;
		padding: 0.25rem;
		border: 1px solid rgba(255, 255, 255, 0.05);
		border-radius: 0.75rem;
		background: rgba(255, 255, 255, 0.025);
	}

	.site-header nav a {
		position: relative;
		padding: 0.55rem 0.75rem;
		border-radius: 0.55rem;
		color: var(--tour-muted);
		font-size: 0.77rem;
		font-weight: 560;
		transition:
			color 160ms ease,
			background 160ms ease;
	}

	.site-header nav a:hover,
	.site-header nav a[aria-current='location'] {
		background: rgba(255, 255, 255, 0.06);
		color: var(--tour-fg);
	}

	.github-link {
		display: inline-flex;
		align-items: center;
		justify-self: end;
		gap: 0.45rem;
		padding: 0.7rem 0.9rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.7rem;
		font-size: 0.78rem;
		font-weight: 650;
		transition:
			border-color 160ms ease,
			background 160ms ease;
	}

	.github-link:hover {
		border-color: rgba(107, 131, 255, 0.5);
		background: var(--tour-accent-soft);
	}

	.menu-button {
		display: none;
	}

	.tour-main,
	footer {
		position: relative;
		z-index: 1;
	}

	.hero {
		position: relative;
		display: grid;
		grid-template-columns: minmax(0, 0.88fr) minmax(34rem, 1.12fr);
		align-items: center;
		min-height: 100vh;
		padding: 9.5rem max(2rem, calc((100vw - 1180px) / 2)) 7rem;
	}

	.hero-grid {
		position: absolute;
		inset: 0;
		background-image:
			linear-gradient(rgba(132, 148, 183, 0.055) 1px, transparent 1px),
			linear-gradient(90deg, rgba(132, 148, 183, 0.055) 1px, transparent 1px);
		background-size: 64px 64px;
		mask-image: linear-gradient(to bottom, black, transparent 76%);
		pointer-events: none;
	}

	.hero-copy {
		position: relative;
		z-index: 4;
		max-width: 40rem;
		animation: reveal-up 700ms both ease-out;
	}

	.eyebrow,
	.kicker {
		color: #9aa9ff;
		font-family: ui-monospace, 'SFMono-Regular', Consolas, monospace;
		font-size: 0.72rem;
		font-weight: 650;
		letter-spacing: 0.12em;
		text-transform: uppercase;
	}

	.eyebrow {
		display: flex;
		align-items: center;
		gap: 0.6rem;
	}

	.eyebrow span {
		width: 1.8rem;
		height: 1px;
		background: var(--tour-accent);
		box-shadow: 0 0 18px var(--tour-accent);
	}

	.hero h1 {
		max-width: 43rem;
		margin: 1.25rem 0 1.5rem;
		font-size: clamp(3.6rem, 6.5vw, 6.9rem);
		font-weight: 680;
		line-height: 0.92;
		letter-spacing: -0.075em;
	}

	.hero h1 span {
		color: #9caafd;
	}

	.hero-lede {
		max-width: 35rem;
		margin: 0;
		color: #b3bdce;
		font-size: clamp(1.02rem, 1.5vw, 1.22rem);
		line-height: 1.7;
	}

	.hero-actions,
	.closing > div {
		display: flex;
		flex-wrap: wrap;
		gap: 0.75rem;
		margin-top: 2rem;
	}

	.button {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 0.6rem;
		min-height: 2.9rem;
		padding: 0.75rem 1rem;
		border: 1px solid transparent;
		border-radius: 0.72rem;
		font-size: 0.82rem;
		font-weight: 680;
		transition:
			transform 160ms ease,
			border-color 160ms ease,
			background 160ms ease;
	}

	.button:hover {
		transform: translateY(-2px);
	}

	.button-primary {
		background: var(--tour-accent);
		box-shadow: 0 12px 40px rgba(76, 98, 224, 0.27);
		color: #070912 !important;
	}

	.button-primary:hover {
		background: #8195ff;
	}

	.button-quiet {
		border-color: var(--tour-line);
		background: rgba(255, 255, 255, 0.025);
	}

	.button-quiet:hover {
		border-color: rgba(107, 131, 255, 0.45);
		background: var(--tour-accent-soft);
	}

	.release-note {
		display: flex;
		align-items: center;
		gap: 0.55rem;
		margin-top: 1.5rem;
		color: var(--tour-muted);
		font-size: 0.74rem;
	}

	.release-note strong {
		color: #c7cfdd;
		font-weight: 650;
	}

	.release-pulse {
		width: 0.45rem;
		height: 0.45rem;
		border-radius: 50%;
		background: #f6b85f;
		box-shadow: 0 0 0 0 rgba(246, 184, 95, 0.45);
		animation: status-pulse 2.4s infinite;
	}

	.hero-visual {
		position: relative;
		z-index: 2;
		width: min(54vw, 52rem);
		margin-left: 2rem;
		perspective: 1600px;
		animation: reveal-window 900ms 120ms both ease-out;
	}

	.product-window {
		position: relative;
		z-index: 3;
		overflow: hidden;
		margin: 0;
		border: 1px solid rgba(151, 166, 196, 0.27);
		border-radius: 1rem;
		background: #0a0d13;
		box-shadow: 0 50px 110px rgba(0, 0, 0, 0.55);
		transform: rotateY(-5deg) rotateX(1.8deg);
		transform-origin: left center;
	}

	.window-bar {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		height: 2.4rem;
		padding: 0 0.75rem;
		border-bottom: 1px solid var(--tour-line);
		background: #10141d;
	}

	.window-bar > span {
		width: 0.42rem;
		height: 0.42rem;
		border-radius: 50%;
		background: #3e475a;
	}

	.window-address {
		margin-left: 0.45rem;
		color: #798499;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.58rem;
	}

	.secure-pill {
		display: flex;
		align-items: center;
		gap: 0.25rem;
		margin-left: auto;
		color: var(--tour-green);
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.54rem;
		text-transform: uppercase;
	}

	.product-window > img {
		display: block;
		width: 100%;
		height: auto;
		filter: saturate(0.82) contrast(1.04);
	}

	.hotspot {
		position: absolute;
		width: 1.5rem;
		height: 1.5rem;
		padding: 0;
		border: 0;
		border-radius: 50%;
		background: rgba(107, 131, 255, 0.18);
		cursor: help;
	}

	.hotspot span {
		position: absolute;
		inset: 0.5rem;
		border-radius: 50%;
		background: #8da0ff;
		box-shadow: 0 0 0 0 rgba(141, 160, 255, 0.7);
		animation: hotspot-pulse 2s infinite;
	}

	.hotspot em {
		position: absolute;
		bottom: calc(100% + 0.55rem);
		left: 50%;
		width: max-content;
		padding: 0.45rem 0.6rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.45rem;
		background: #111621;
		box-shadow: 0 8px 30px rgba(0, 0, 0, 0.4);
		color: var(--tour-fg);
		font-size: 0.62rem;
		font-style: normal;
		opacity: 0;
		transform: translate(-50%, 0.25rem);
		transition:
			opacity 140ms ease,
			transform 140ms ease;
		pointer-events: none;
	}

	.hotspot:hover em,
	.hotspot:focus-visible em {
		opacity: 1;
		transform: translate(-50%, 0);
	}

	.hotspot-search {
		top: 12%;
		left: 42%;
	}

	.hotspot-rights {
		top: 39%;
		right: 5%;
	}

	.hotspot-proof {
		top: 49%;
		right: 6%;
	}

	.visual-orbit {
		position: absolute;
		z-index: 1;
		border: 1px solid rgba(107, 131, 255, 0.16);
		border-radius: 50%;
		pointer-events: none;
	}

	.orbit-one {
		top: -8%;
		left: -8%;
		width: 112%;
		aspect-ratio: 1;
		animation: orbit-spin 34s linear infinite;
	}

	.orbit-two {
		top: 8%;
		left: 5%;
		width: 88%;
		aspect-ratio: 1;
		border-style: dashed;
		animation: orbit-spin 27s reverse linear infinite;
	}

	.floating-card {
		position: absolute;
		z-index: 6;
		display: flex;
		align-items: center;
		gap: 0.65rem;
		min-width: 11.5rem;
		padding: 0.7rem;
		border: 1px solid rgba(151, 166, 196, 0.25);
		border-radius: 0.75rem;
		background: rgba(17, 22, 33, 0.93);
		box-shadow: 0 20px 55px rgba(0, 0, 0, 0.42);
		backdrop-filter: blur(12px);
	}

	.floating-card span {
		display: block;
		margin-bottom: 0.15rem;
		color: var(--tour-muted);
		font-size: 0.57rem;
		text-transform: uppercase;
		letter-spacing: 0.08em;
	}

	.floating-card strong {
		display: block;
		font-size: 0.7rem;
		font-weight: 630;
	}

	.card-icon {
		display: grid;
		width: 2rem;
		height: 2rem;
		place-items: center;
		border-radius: 0.55rem;
		background: var(--tour-accent-soft);
		color: #91a2ff;
	}

	.card-icon.ok {
		background: rgba(102, 213, 154, 0.12);
		color: var(--tour-green);
	}

	.delivery-card {
		right: -2.4rem;
		bottom: 14%;
		animation: float 5s ease-in-out infinite;
	}

	.provenance-card {
		bottom: -1.6rem;
		left: 12%;
		animation: float 5.8s 900ms ease-in-out infinite;
	}

	.hero-facts {
		position: absolute;
		bottom: 1.8rem;
		left: 50%;
		display: grid;
		grid-template-columns: repeat(5, 1fr);
		width: min(1180px, calc(100% - 4rem));
		border-top: 1px solid var(--tour-line);
		transform: translateX(-50%);
	}

	.hero-facts div {
		display: flex;
		align-items: baseline;
		gap: 0.5rem;
		padding: 1.1rem 1rem 0;
		border-left: 1px solid var(--tour-line);
	}

	.hero-facts div:last-child {
		border-right: 1px solid var(--tour-line);
	}

	.hero-facts strong {
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 1rem;
		font-weight: 600;
	}

	.hero-facts span {
		color: var(--tour-muted);
		font-size: 0.68rem;
	}

	.section {
		width: min(1180px, calc(100% - 4rem));
		margin: 0 auto;
		padding: 8rem 0;
		border-top: 1px solid var(--tour-line);
	}

	.section-heading {
		max-width: 48rem;
		margin-bottom: 3.5rem;
	}

	.split-heading {
		display: grid;
		grid-template-columns: 1.2fr 0.8fr;
		gap: 5rem;
		max-width: none;
		align-items: end;
	}

	.section-heading h2,
	.closing h2 {
		margin: 0.75rem 0 0;
		font-size: clamp(2.6rem, 5vw, 4.8rem);
		font-weight: 620;
		line-height: 1;
		letter-spacing: -0.06em;
	}

	.section-heading > p:not(.kicker),
	.split-heading > p {
		margin: 1rem 0 0;
		color: var(--tour-muted);
		font-size: 0.96rem;
		line-height: 1.7;
	}

	.split-heading > p {
		margin-bottom: 0.2rem;
	}

	.gate-lab {
		display: grid;
		grid-template-columns: 1fr 13rem 1fr;
		align-items: stretch;
		min-height: 26rem;
		overflow: hidden;
		border: 1px solid var(--tour-line);
		border-radius: 1.25rem;
		background:
			linear-gradient(130deg, rgba(107, 131, 255, 0.04), transparent 48%), var(--tour-surface);
	}

	.gate-controls,
	.gate-result {
		padding: 2rem;
	}

	.gate-controls {
		display: flex;
		flex-direction: column;
		justify-content: center;
		gap: 1.75rem;
	}

	.control-label {
		display: block;
		margin-bottom: 0.7rem;
		color: var(--tour-muted);
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.63rem;
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.segmented,
	.rights-options {
		display: grid;
		gap: 0.35rem;
		padding: 0.3rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.65rem;
		background: rgba(4, 6, 10, 0.42);
	}

	.segmented {
		grid-template-columns: 1fr 1fr;
	}

	.rights-options {
		grid-template-columns: repeat(3, 1fr);
	}

	.segmented button,
	.rights-options button,
	.docs-tabs button,
	.product-tabs button,
	.code-bar button {
		border: 0;
		color: var(--tour-muted);
		font: inherit;
		cursor: pointer;
	}

	.segmented button,
	.rights-options button {
		min-height: 2.5rem;
		border-radius: 0.45rem;
		background: transparent;
		font-size: 0.72rem;
		transition:
			background 150ms ease,
			color 150ms ease;
	}

	.segmented button.active,
	.rights-options button.active {
		background: var(--tour-raised);
		box-shadow: 0 5px 16px rgba(0, 0, 0, 0.18);
		color: var(--tour-fg);
	}

	.rights-options button {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 0.4rem;
	}

	.dot {
		width: 0.42rem;
		height: 0.42rem;
		border-radius: 50%;
	}

	.dot.cleared {
		background: var(--tour-green);
	}

	.dot.unknown {
		background: #a998e9;
	}

	.dot.expired {
		background: var(--tour-red);
	}

	.request-packet {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		padding: 0.9rem 1rem;
		border: 1px solid rgba(107, 131, 255, 0.25);
		border-radius: 0.6rem;
		background: var(--tour-accent-soft);
	}

	.request-packet > span {
		font-size: 0.72rem;
		font-weight: 650;
	}

	.request-packet code,
	.gate-result code {
		color: #9dacff;
		font-family: ui-monospace, 'SFMono-Regular', Consolas, monospace;
		font-size: 0.6rem;
	}

	.packet-line {
		position: absolute;
		top: 50%;
		left: 100%;
		width: calc(13rem + 1px);
		height: 1px;
		overflow: hidden;
		background: rgba(107, 131, 255, 0.23);
	}

	.packet-line i {
		position: absolute;
		top: 0;
		width: 35%;
		height: 1px;
		background: var(--tour-accent);
		box-shadow: 0 0 10px var(--tour-accent);
		animation: packet-travel 1.8s infinite linear;
	}

	.gate-core {
		position: relative;
		display: grid;
		place-items: center;
		border-right: 1px solid var(--tour-line);
		border-left: 1px solid var(--tour-line);
		background: rgba(5, 8, 13, 0.55);
		color: var(--tour-green);
		transition: color 240ms ease;
	}

	.gate-core.denied {
		color: var(--tour-red);
	}

	.gate-core > span {
		position: absolute;
		bottom: 2.25rem;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.56rem;
		letter-spacing: 0.14em;
	}

	.gate-mark {
		position: relative;
		z-index: 3;
		display: grid;
		width: 5rem;
		height: 5rem;
		place-items: center;
		border: 1px solid currentColor;
		border-radius: 50%;
		background: #0b0e15;
		box-shadow: 0 0 45px color-mix(in srgb, currentColor 17%, transparent);
	}

	.gate-ring {
		position: absolute;
		border: 1px solid currentColor;
		border-radius: 50%;
		opacity: 0.16;
	}

	.ring-a {
		width: 8rem;
		height: 8rem;
		animation: gate-spin 10s infinite linear;
	}

	.ring-b {
		width: 11rem;
		height: 11rem;
		border-style: dashed;
		animation: gate-spin 16s infinite reverse linear;
	}

	.gate-result {
		display: flex;
		flex-direction: column;
		justify-content: center;
		background: linear-gradient(150deg, rgba(102, 213, 154, 0.075), transparent 62%);
		transition: background 240ms ease;
	}

	.gate-result.denied {
		background: linear-gradient(150deg, rgba(255, 121, 109, 0.08), transparent 62%);
	}

	.decision-badge {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		width: max-content;
		padding: 0.35rem 0.55rem;
		border-radius: 999px;
		background: rgba(102, 213, 154, 0.1);
		color: var(--tour-green);
		font-size: 0.64rem;
		font-weight: 650;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}

	.denied .decision-badge {
		background: rgba(255, 121, 109, 0.1);
		color: var(--tour-red);
	}

	.gate-result h3 {
		margin: 1.1rem 0 0.65rem;
		font-size: 1.55rem;
		letter-spacing: -0.04em;
	}

	.gate-result > p {
		min-height: 4.25rem;
		margin: 0 0 1rem;
		color: var(--tour-muted);
		font-size: 0.82rem;
		line-height: 1.65;
	}

	.gate-result > code {
		display: block;
		min-height: 3.7rem;
		padding: 0.8rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.5rem;
		background: rgba(0, 0, 0, 0.26);
		line-height: 1.6;
	}

	.result-footer {
		display: flex;
		justify-content: space-between;
		margin-top: 0.8rem;
		color: #788397;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.56rem;
	}

	.result-footer span:first-child {
		display: flex;
		align-items: center;
		gap: 0.35rem;
	}

	.product-section .section-heading {
		margin-right: auto;
		margin-left: auto;
		text-align: center;
	}

	.product-explorer {
		overflow: hidden;
		border: 1px solid var(--tour-line);
		border-radius: 1.25rem;
		background: var(--tour-surface);
	}

	.product-tabs {
		display: grid;
		grid-template-columns: repeat(4, 1fr);
		border-bottom: 1px solid var(--tour-line);
	}

	.product-tabs button {
		position: relative;
		display: flex;
		align-items: center;
		gap: 0.7rem;
		min-height: 4.6rem;
		padding: 0 1.2rem;
		border-right: 1px solid var(--tour-line);
		background: transparent;
		text-align: left;
	}

	.product-tabs button:last-child {
		border-right: 0;
	}

	.product-tabs button::after {
		position: absolute;
		right: 0;
		bottom: -1px;
		left: 0;
		height: 2px;
		background: var(--tour-accent);
		content: '';
		transform: scaleX(0);
		transition: transform 180ms ease;
	}

	.product-tabs button.active {
		background: rgba(107, 131, 255, 0.045);
		color: var(--tour-fg);
	}

	.product-tabs button.active::after {
		transform: scaleX(1);
	}

	.product-tabs button span {
		color: #6f7a8e;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.62rem;
	}

	.product-tabs button strong {
		font-size: 0.82rem;
		font-weight: 650;
	}

	.product-panel {
		display: grid;
		grid-template-columns: 0.78fr 1.22fr;
		min-height: 31rem;
	}

	.product-copy {
		display: flex;
		flex-direction: column;
		justify-content: center;
		padding: 3.5rem;
	}

	.mode-label {
		margin: 0;
		color: #98a7ff;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.67rem;
		font-weight: 650;
		letter-spacing: 0.1em;
		text-transform: uppercase;
	}

	.product-copy h3 {
		margin: 0.9rem 0 1rem;
		font-size: clamp(2rem, 3vw, 3rem);
		font-weight: 620;
		line-height: 1.05;
		letter-spacing: -0.05em;
	}

	.product-copy > p:not(.mode-label) {
		margin: 0;
		color: var(--tour-muted);
		font-size: 0.9rem;
		line-height: 1.7;
	}

	.product-copy ul {
		display: grid;
		gap: 0.75rem;
		padding: 1.6rem 0 0;
		margin: 1.6rem 0 0;
		border-top: 1px solid var(--tour-line);
		list-style: none;
	}

	.product-copy li {
		display: flex;
		align-items: center;
		gap: 0.55rem;
		font-size: 0.78rem;
	}

	.product-copy li :global(svg) {
		color: var(--tour-green);
	}

	.product-crop {
		position: relative;
		overflow: hidden;
		min-height: 31rem;
		border-left: 1px solid var(--tour-line);
		background: #080a0f;
	}

	.product-crop::after {
		position: absolute;
		inset: 0;
		background: linear-gradient(90deg, rgba(15, 19, 28, 0.62), transparent 22%);
		content: '';
		pointer-events: none;
	}

	.product-crop img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		filter: saturate(0.8) contrast(1.07);
		animation: crop-arrive 350ms both ease-out;
	}

	.crop-label {
		position: absolute;
		right: 1rem;
		bottom: 1rem;
		z-index: 2;
		display: flex;
		align-items: center;
		gap: 0.45rem;
		padding: 0.45rem 0.6rem;
		border: 1px solid var(--tour-line);
		border-radius: 999px;
		background: rgba(8, 10, 15, 0.8);
		color: #c1cad9;
		font-size: 0.6rem;
		backdrop-filter: blur(10px);
	}

	.crop-label span {
		width: 0.4rem;
		height: 0.4rem;
		border-radius: 50%;
		background: var(--tour-green);
	}

	.capability-strip {
		display: grid;
		grid-template-columns: repeat(6, 1fr);
		margin-top: 1.2rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.9rem;
		background: rgba(15, 19, 28, 0.55);
	}

	.capability-strip div {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 0.6rem;
		min-height: 4.6rem;
		border-right: 1px solid var(--tour-line);
		color: #9da8bb;
		font-size: 0.72rem;
	}

	.capability-strip div:last-child {
		border-right: 0;
	}

	.capability-strip :global(svg) {
		color: #8396ff;
	}

	.architecture-map {
		position: relative;
		display: grid;
		grid-template-columns: repeat(4, 1fr);
		gap: 1rem;
		padding: 2rem;
		border: 1px solid var(--tour-line);
		border-radius: 1.25rem;
		background:
			radial-gradient(circle at 50% 20%, rgba(107, 131, 255, 0.08), transparent 38%),
			var(--tour-surface);
	}

	.flow-line {
		position: absolute;
		top: 6.5rem;
		left: 10%;
		width: 80%;
		height: 1px;
		overflow: hidden;
		background: rgba(107, 131, 255, 0.23);
	}

	.flow-line span {
		position: absolute;
		width: 12%;
		height: 1px;
		background: var(--tour-accent);
		box-shadow: 0 0 14px var(--tour-accent);
		animation: flow-run 3s linear infinite;
	}

	.arch-node {
		position: relative;
		z-index: 2;
		min-height: 13rem;
		padding: 1.2rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.85rem;
		background: rgba(8, 10, 15, 0.78);
	}

	.node-icon {
		display: grid;
		width: 3.4rem;
		height: 3.4rem;
		margin-bottom: 2.25rem;
		place-items: center;
		border: 1px solid rgba(107, 131, 255, 0.3);
		border-radius: 0.85rem;
		background: #101522;
		box-shadow: 0 0 0 7px var(--tour-surface);
		color: #91a2ff;
	}

	.arch-node > span,
	.architecture-notes article > span,
	.doc-cards > a > span {
		color: #7f8ba0;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.61rem;
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	.arch-node h3 {
		margin: 0.4rem 0;
		font-size: 1rem;
		font-weight: 650;
	}

	.arch-node p {
		margin: 0;
		color: var(--tour-muted);
		font-size: 0.7rem;
		line-height: 1.55;
	}

	.storage-rail {
		grid-column: 1 / -1;
		display: grid;
		grid-template-columns: 1.15fr 1fr 1fr 1fr;
		align-items: center;
		overflow: hidden;
		margin-top: 0.5rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.75rem;
		background: #090c12;
	}

	.storage-title,
	.tier {
		min-height: 4.2rem;
		padding: 0.8rem 1rem;
	}

	.storage-title {
		display: flex;
		align-items: center;
		gap: 0.55rem;
		border-right: 1px solid var(--tour-line);
		font-size: 0.7rem;
		font-weight: 620;
	}

	.tier {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 0.25rem 0.55rem;
		border-right: 1px solid var(--tour-line);
	}

	.tier:last-child {
		border-right: 0;
	}

	.tier span {
		grid-row: 1 / 3;
		align-self: center;
		padding: 0.25rem 0.35rem;
		border-radius: 0.35rem;
		background: rgba(107, 131, 255, 0.12);
		color: #98a7ff;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.52rem;
	}

	.tier.cool span {
		background: rgba(115, 201, 205, 0.1);
		color: #7ac9cf;
	}

	.tier.archive span {
		background: rgba(169, 152, 233, 0.1);
		color: #b4a5ed;
	}

	.tier strong {
		font-size: 0.62rem;
		font-weight: 600;
	}

	.tier em {
		color: #778195;
		font-size: 0.55rem;
		font-style: normal;
	}

	.architecture-notes {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 1px;
		margin-top: 1.2rem;
		overflow: hidden;
		border: 1px solid var(--tour-line);
		border-radius: 0.9rem;
		background: var(--tour-line);
	}

	.architecture-notes article {
		min-height: 13rem;
		padding: 1.6rem;
		background: var(--tour-bg);
	}

	.architecture-notes h3 {
		margin: 1.5rem 0 0.6rem;
		font-size: 1rem;
		font-weight: 630;
	}

	.architecture-notes p {
		margin: 0;
		color: var(--tour-muted);
		font-size: 0.76rem;
		line-height: 1.65;
	}

	.docs-console {
		overflow: hidden;
		border: 1px solid var(--tour-line);
		border-radius: 1.25rem;
		background: var(--tour-surface);
	}

	.docs-tabs {
		display: flex;
		gap: 0.2rem;
		padding: 0.55rem;
		border-bottom: 1px solid var(--tour-line);
	}

	.docs-tabs button {
		padding: 0.65rem 0.85rem;
		border-radius: 0.5rem;
		background: transparent;
		font-size: 0.73rem;
	}

	.docs-tabs button.active {
		background: var(--tour-raised);
		color: var(--tour-fg);
	}

	.docs-panel {
		display: grid;
		grid-template-columns: 0.85fr 1.15fr;
		gap: 4rem;
		padding: 3rem;
	}

	.docs-copy {
		align-self: center;
	}

	.doc-icon {
		display: grid;
		width: 3rem;
		height: 3rem;
		place-items: center;
		border: 1px solid rgba(107, 131, 255, 0.25);
		border-radius: 0.7rem;
		background: var(--tour-accent-soft);
		color: #91a2ff;
	}

	.docs-copy h3 {
		margin: 1.3rem 0 0.7rem;
		font-size: 1.8rem;
		font-weight: 620;
		letter-spacing: -0.04em;
	}

	.docs-copy > p {
		margin: 0;
		color: var(--tour-muted);
		font-size: 0.82rem;
		line-height: 1.7;
	}

	.doc-links {
		display: flex;
		flex-wrap: wrap;
		gap: 0.6rem 1rem;
		margin-top: 1.4rem;
	}

	.doc-links a {
		display: inline-flex;
		align-items: center;
		gap: 0.25rem;
		color: #a9b5ff;
		font-size: 0.72rem;
		font-weight: 620;
	}

	.doc-links a.full-guide {
		padding: 0.42rem 0.6rem;
		border: 1px solid rgba(107, 131, 255, 0.28);
		border-radius: 0.45rem;
		background: var(--tour-accent-soft);
		color: #bec7ff;
	}

	.doc-links a:hover {
		text-decoration: underline;
	}

	.code-window {
		overflow: hidden;
		min-height: 16rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.85rem;
		background: #07090d;
		box-shadow: 0 24px 55px rgba(0, 0, 0, 0.28);
	}

	.code-bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		min-height: 2.8rem;
		padding: 0 0.8rem;
		border-bottom: 1px solid var(--tour-line);
		background: #0d1118;
		color: #8994a8;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.61rem;
	}

	.code-bar span {
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}

	.code-bar button {
		padding: 0.35rem 0.5rem;
		border-radius: 0.35rem;
		background: rgba(255, 255, 255, 0.05);
		font-size: 0.59rem;
	}

	.code-bar button:hover {
		background: var(--tour-accent-soft);
		color: #afbbff;
	}

	.code-window pre {
		padding: 1.5rem;
		margin: 0;
		white-space: pre-wrap;
	}

	.code-window code {
		color: #bdc6d6;
		font-family: ui-monospace, 'SFMono-Regular', Consolas, monospace;
		font-size: 0.72rem;
		line-height: 1.9;
	}

	.doc-cards {
		display: grid;
		grid-template-columns: repeat(3, 1fr);
		gap: 1rem;
		margin-top: 1rem;
	}

	.doc-cards > a {
		position: relative;
		min-height: 11rem;
		padding: 1.5rem;
		border: 1px solid var(--tour-line);
		border-radius: 0.9rem;
		background: rgba(15, 19, 28, 0.55);
		transition:
			transform 160ms ease,
			border-color 160ms ease,
			background 160ms ease;
	}

	.doc-cards > a:hover {
		border-color: rgba(107, 131, 255, 0.4);
		background: var(--tour-surface);
		transform: translateY(-3px);
	}

	.doc-cards h3 {
		max-width: 15rem;
		margin: 1.5rem 0 0;
		font-size: 1.05rem;
		font-weight: 620;
		line-height: 1.35;
		letter-spacing: -0.025em;
	}

	.doc-cards :global(svg) {
		position: absolute;
		right: 1.25rem;
		bottom: 1.25rem;
		color: #8999f8;
	}

	.closing {
		display: flex;
		width: min(1180px, calc(100% - 4rem));
		min-height: 38rem;
		margin: 0 auto 2rem;
		padding: 7rem;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		border: 1px solid var(--tour-line);
		border-radius: 1.5rem;
		background:
			radial-gradient(circle at 50% 0%, rgba(107, 131, 255, 0.19), transparent 47%),
			linear-gradient(rgba(255, 255, 255, 0.015), rgba(255, 255, 255, 0.015)), var(--tour-surface);
		text-align: center;
	}

	.closing > img {
		width: 2.2rem;
		height: 2.8rem;
		margin-bottom: 1.2rem;
	}

	.closing > p {
		margin: 0;
		color: #9dacff;
		font-family: ui-monospace, 'SFMono-Regular', monospace;
		font-size: 0.68rem;
		letter-spacing: 0.12em;
		text-transform: uppercase;
	}

	.closing h2 {
		max-width: 53rem;
		font-size: clamp(2.5rem, 5vw, 5rem);
	}

	.closing > div {
		justify-content: center;
	}

	footer {
		display: grid;
		grid-template-columns: 1fr auto 1fr;
		align-items: center;
		width: min(1180px, calc(100% - 4rem));
		min-height: 8rem;
		margin: 0 auto;
		border-top: 1px solid var(--tour-line);
		color: var(--tour-muted);
		font-size: 0.66rem;
	}

	.footer-brand {
		color: var(--tour-fg) !important;
	}

	footer > p {
		margin: 0;
	}

	footer > div {
		display: flex;
		align-items: center;
		justify-self: end;
		gap: 1.25rem;
	}

	footer > div span {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		color: #d0a35f;
	}

	footer a:hover {
		color: var(--tour-fg);
	}

	@keyframes reveal-up {
		from {
			transform: translateY(1.5rem);
		}
		to {
			transform: translateY(0);
		}
	}

	@keyframes reveal-window {
		from {
			transform: translateX(3rem) scale(0.97);
		}
		to {
			transform: translateX(0) scale(1);
		}
	}

	@keyframes float {
		0%,
		100% {
			transform: translateY(0);
		}
		50% {
			transform: translateY(-0.65rem);
		}
	}

	@keyframes orbit-spin {
		to {
			transform: rotate(360deg);
		}
	}

	@keyframes status-pulse {
		70% {
			box-shadow: 0 0 0 0.45rem rgba(246, 184, 95, 0);
		}
		100% {
			box-shadow: 0 0 0 0 rgba(246, 184, 95, 0);
		}
	}

	@keyframes hotspot-pulse {
		70% {
			box-shadow: 0 0 0 0.6rem rgba(141, 160, 255, 0);
		}
		100% {
			box-shadow: 0 0 0 0 rgba(141, 160, 255, 0);
		}
	}

	@keyframes packet-travel {
		from {
			left: -35%;
		}
		to {
			left: 100%;
		}
	}

	@keyframes gate-spin {
		to {
			transform: rotate(360deg);
		}
	}

	@keyframes flow-run {
		from {
			left: -12%;
		}
		to {
			left: 100%;
		}
	}

	@keyframes crop-arrive {
		from {
			opacity: 0.5;
			transform: scale(1.025);
		}
		to {
			opacity: 1;
			transform: scale(1);
		}
	}

	@media (max-width: 1050px) {
		.site-header {
			grid-template-columns: 1fr auto;
		}

		.site-header nav {
			display: none;
			position: absolute;
			top: calc(100% + 0.5rem);
			right: 0;
			left: 0;
			padding: 0.6rem;
			border: 1px solid var(--tour-line);
			background: rgba(8, 10, 15, 0.96);
			box-shadow: 0 18px 55px rgba(0, 0, 0, 0.45);
			backdrop-filter: blur(20px);
		}

		.site-header nav.open {
			display: grid;
		}

		.menu-button {
			display: flex;
			grid-column: 2;
			grid-row: 1;
			width: 2.5rem;
			height: 2.5rem;
			padding: 0;
			border: 1px solid var(--tour-line);
			border-radius: 0.6rem;
			background: transparent;
			flex-direction: column;
			align-items: center;
			justify-content: center;
			gap: 0.35rem;
		}

		.menu-button span {
			width: 0.9rem;
			height: 1px;
			background: var(--tour-fg);
		}

		.github-link {
			display: none;
		}

		.hero {
			grid-template-columns: 1fr;
			gap: 5rem;
			padding-top: 10rem;
		}

		.hero-copy {
			max-width: 48rem;
		}

		.hero-visual {
			width: calc(100% - 2rem);
			margin: 0 auto 6rem;
		}

		.hero-facts {
			bottom: 1rem;
		}

		.gate-lab {
			grid-template-columns: 1fr 10rem 1fr;
		}

		.packet-line {
			width: 10rem;
		}
	}

	@media (max-width: 760px) {
		.hero,
		.section,
		.closing,
		footer {
			width: calc(100% - 2rem);
		}

		.hero {
			padding-right: 0;
			padding-left: 0;
		}

		.hero h1 {
			font-size: clamp(3.25rem, 15vw, 5.4rem);
		}

		.release-note span:last-child {
			display: none;
		}

		.hero-visual {
			width: 100%;
		}

		.product-window {
			transform: none;
		}

		.floating-card {
			display: none;
		}

		.hero-facts {
			position: relative;
			bottom: auto;
			left: auto;
			grid-template-columns: repeat(2, 1fr);
			width: 100%;
			margin-top: 1rem;
			transform: none;
		}

		.hero-facts div:last-child {
			grid-column: 1 / -1;
			border-right: 1px solid var(--tour-line);
		}

		.section {
			padding: 6rem 0;
		}

		.split-heading {
			grid-template-columns: 1fr;
			gap: 1rem;
		}

		.gate-lab {
			grid-template-columns: 1fr;
		}

		.gate-core {
			min-height: 15rem;
			border-top: 1px solid var(--tour-line);
			border-right: 0;
			border-bottom: 1px solid var(--tour-line);
			border-left: 0;
		}

		.packet-line {
			display: none;
		}

		.product-tabs {
			grid-template-columns: 1fr 1fr;
		}

		.product-tabs button:nth-child(2) {
			border-right: 0;
		}

		.product-tabs button:nth-child(-n + 2) {
			border-bottom: 1px solid var(--tour-line);
		}

		.product-panel {
			grid-template-columns: 1fr;
		}

		.product-copy {
			padding: 2rem;
		}

		.product-crop {
			min-height: 24rem;
			border-top: 1px solid var(--tour-line);
			border-left: 0;
		}

		.capability-strip {
			grid-template-columns: repeat(2, 1fr);
		}

		.capability-strip div {
			border-bottom: 1px solid var(--tour-line);
		}

		.capability-strip div:nth-child(even) {
			border-right: 0;
		}

		.capability-strip div:nth-last-child(-n + 2) {
			border-bottom: 0;
		}

		.architecture-map {
			grid-template-columns: 1fr 1fr;
		}

		.flow-line {
			display: none;
		}

		.storage-rail {
			grid-template-columns: 1fr;
		}

		.storage-title,
		.tier {
			border-right: 0;
			border-bottom: 1px solid var(--tour-line);
		}

		.tier:last-child {
			border-bottom: 0;
		}

		.architecture-notes,
		.doc-cards {
			grid-template-columns: 1fr;
		}

		.docs-panel {
			grid-template-columns: 1fr;
			gap: 2rem;
			padding: 2rem;
		}

		.docs-tabs {
			overflow-x: auto;
		}

		.closing {
			min-height: 34rem;
			padding: 3rem 1.5rem;
		}

		footer {
			grid-template-columns: 1fr;
			gap: 1rem;
			padding: 2rem 0;
		}

		footer > div {
			justify-self: start;
		}
	}

	@media (max-width: 480px) {
		.hero h1 {
			font-size: 3.35rem;
		}

		.hero-actions,
		.closing > div {
			flex-direction: column;
		}

		.rights-options {
			grid-template-columns: 1fr;
		}

		.architecture-map {
			grid-template-columns: 1fr;
			padding: 1rem;
		}

		.product-tabs button {
			padding: 0 0.8rem;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		:global(html) {
			scroll-behavior: auto;
		}

		.hero-copy,
		.hero-visual,
		.release-pulse,
		.hotspot span,
		.visual-orbit,
		.floating-card,
		.packet-line i,
		.gate-ring,
		.flow-line span,
		.product-crop img {
			animation: none !important;
		}
	}
</style>
