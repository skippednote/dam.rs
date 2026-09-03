export type DocSlug =
	| 'getting-started'
	| 'architecture'
	| 'ingest-delivery'
	| 'api-mcp'
	| 'operations'
	| 'security'
	| 'contributing';

export type DocHref =
	| '/tour/docs'
	| '/tour/docs/architecture'
	| '/tour/docs/ingest-delivery'
	| '/tour/docs/api-mcp'
	| '/tour/docs/operations'
	| '/tour/docs/security'
	| '/tour/docs/contributing';

type ParagraphBlock = {
	kind: 'paragraphs';
	content: string[];
};

type ListBlock = {
	kind: 'list';
	items: string[];
};

type StepsBlock = {
	kind: 'steps';
	items: Array<{ title: string; body: string }>;
};

type CodeBlock = {
	kind: 'code';
	title: string;
	language: string;
	code: string;
};

type CalloutBlock = {
	kind: 'callout';
	tone: 'note' | 'warning' | 'success';
	title: string;
	body: string;
};

type TableBlock = {
	kind: 'table';
	headers: string[];
	rows: string[][];
};

export type DocBlock =
	ParagraphBlock | ListBlock | StepsBlock | CodeBlock | CalloutBlock | TableBlock;

export type DocSection = {
	id: string;
	title: string;
	lede?: string;
	blocks: DocBlock[];
};

export type DocPage = {
	slug: DocSlug;
	href: DocHref;
	nav: string;
	title: string;
	description: string;
	category: string;
	readingTime: string;
	keywords: string[];
	source: { label: string; href: string };
	sections: DocSection[];
};

export const DOC_ORDER: DocSlug[] = [
	'getting-started',
	'architecture',
	'ingest-delivery',
	'api-mcp',
	'operations',
	'security',
	'contributing'
];

export const DOC_GROUPS: Array<{ label: string; items: DocSlug[] }> = [
	{ label: 'Start here', items: ['getting-started', 'architecture'] },
	{ label: 'Build with dam.rs', items: ['ingest-delivery', 'api-mcp'] },
	{ label: 'Run it', items: ['operations', 'security', 'contributing'] }
];

export const DOCS: Record<DocSlug, DocPage> = {
	'getting-started': {
		slug: 'getting-started',
		href: '/tour/docs',
		nav: 'Getting started',
		title: 'Get a local library running',
		description:
			'Install the pinned toolchain, start the stateful services, seed a tenant and connect the operator interface.',
		category: 'Start here',
		readingTime: '6 min',
		keywords: ['install', 'local', 'mise', 'docker', 'seed', 'api key', 'frontend', 'worker'],
		source: {
			label: 'README',
			href: 'https://github.com/skippednote/dam.rs/blob/main/README.md'
		},
		sections: [
			{
				id: 'before-you-begin',
				title: 'Before you begin',
				lede: 'The development environment is deliberately reproducible.',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'dam.rs uses <code>mise</code> for the Rust, Node, PostgreSQL client and media toolchain versions. Docker runs PostgreSQL with pgvector and SeaweedFS, the local S3-compatible object store.',
							'You need <code>mise</code>, Docker with Compose, and enough disk space for images, build output and local object storage. The repository does not assume a globally installed Rust or Node version.'
						]
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'The worker is part of the product',
						body: 'An upload remains in staging until the worker verifies, promotes, derives and indexes it. Running only the API and web app produces a library that looks broken.'
					}
				]
			},
			{
				id: 'install-and-seed',
				title: 'Install and seed',
				blocks: [
					{
						kind: 'steps',
						items: [
							{
								title: 'Install the pinned dependencies',
								body: 'Run mise from the repository root. It installs the versions described by the project rather than whatever happens to be on the machine.'
							},
							{
								title: 'Start PostgreSQL and object storage',
								body: 'The development Compose file starts the services and health checks expected by the API and worker.'
							},
							{
								title: 'Create the development tenant',
								body: 'The seed task migrates the database and prints a development API key once. Treat the key as a credential even though it belongs only to the local stack.'
							}
						]
					},
					{
						kind: 'code',
						title: 'Repository root',
						language: 'shell',
						code: ['mise install', 'mise run up', 'mise run dev:seed'].join('\n')
					}
				]
			},
			{
				id: 'start-the-system',
				title: 'Start the three processes',
				lede: 'Keep each long-running process in its own terminal.',
				blocks: [
					{
						kind: 'code',
						title: 'Terminal 1 · API',
						language: 'shell',
						code: 'mise run dev:api'
					},
					{
						kind: 'code',
						title: 'Terminal 2 · worker',
						language: 'shell',
						code: 'mise run dev:worker'
					},
					{
						kind: 'code',
						title: 'Terminal 3 · Svelte app',
						language: 'shell',
						code: 'mise run dev:web'
					}
				]
			},
			{
				id: 'connect-and-verify',
				title: 'Connect and verify',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Open the URL printed by Vite, go to <strong>Settings</strong>, and paste the key from <code>dev:seed</code>. The development client keeps that key in this browser only.',
							'Visit <strong>Assets</strong> after connecting. A new tenant is expected to be empty. Upload a small image and watch it move from the upload queue into the grid; that proves the API, object store, worker, derivative path and search index are all participating.'
						]
					},
					{
						kind: 'code',
						title: 'Fast health check',
						language: 'shell',
						code: [
							'curl -fsS http://127.0.0.1:8080/health',
							'curl -fsS http://127.0.0.1:8080/ready'
						].join('\n')
					},
					{
						kind: 'callout',
						tone: 'note',
						title: 'Development authentication is intentionally temporary',
						body: 'The current operator interface uses bearer API keys. Human login and SSO remain production prerequisites, not capabilities implied by the local setup.'
					}
				]
			}
		]
	},
	architecture: {
		slug: 'architecture',
		href: '/tour/docs/architecture',
		nav: 'Architecture',
		title: 'Architecture and invariants',
		description:
			'Understand the boundaries that make rights enforcement, tenant isolation, archival storage and recovery dependable.',
		category: 'Start here',
		readingTime: '9 min',
		keywords: ['architecture', 'tenancy', 'postgresql', 'tantivy', 'worker', 's3', 'invariants'],
		source: {
			label: 'ARCHITECTURE.md',
			href: 'https://github.com/skippednote/dam.rs/blob/main/ARCHITECTURE.md'
		},
		sections: [
			{
				id: 'load-bearing-decisions',
				title: 'The load-bearing decisions',
				blocks: [
					{
						kind: 'list',
						items: [
							'<strong>Rights are enforced at distribution.</strong> A signed URL permits an attempt; current evidence still decides whether bytes leave.',
							'<strong>Search never touches the original blob.</strong> Metadata, text, vectors and generous proxies are extracted while bytes are hot.',
							'<strong>Each tenant is a PostgreSQL schema.</strong> Requests enter through a transaction-scoped connection with its search path fixed.',
							'<strong>Search is derived state.</strong> PostgreSQL is the record; Tantivy and pgvector can be rebuilt.',
							'<strong>Processes split by failure profile.</strong> The API stays latency-sensitive while workers own CPU-heavy and untrusted media work.'
						]
					},
					{
						kind: 'callout',
						tone: 'success',
						title: 'What archival changes',
						body: 'Only original download latency changes. Keyword search, facets, semantic search, previews and extracted text remain available at every storage tier.'
					}
				]
			},
			{
				id: 'runtime-topology',
				title: 'Runtime topology',
				blocks: [
					{
						kind: 'code',
						title: 'Processes and state',
						language: 'text',
						code: [
							'Browser / Drupal / MCP',
							'          │',
							'          ▼',
							'       damd API ───── PostgreSQL',
							'          │             ├─ global registry',
							'          │             └─ schema per tenant',
							'          ├────────── S3-compatible storage',
							'          └────────── durable jobs',
							'                           │',
							'                           ▼',
							'                      dam-worker',
							'                      ├─ verify + derive',
							'                      ├─ enrich + index',
							'                      └─ tier + restore'
						].join('\n')
					},
					{
						kind: 'table',
						headers: ['Component', 'Owns', 'Does not own'],
						rows: [
							['damd', 'REST, MCP, authorization, delivery decisions', 'Long-running media work'],
							['dam-worker', 'Jobs, derivatives, indexing, lifecycle', 'Public HTTP traffic'],
							['PostgreSQL', 'Authoritative metadata and durable queue', 'Original media bytes'],
							['Object storage', 'Originals, derivatives and manifests', 'Authorization decisions'],
							['Tantivy + pgvector', 'Fast derived retrieval', 'Authoritative state']
						]
					}
				]
			},
			{
				id: 'tenant-boundary',
				title: 'The tenant boundary',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Tenant data lives in schemas such as <code>t_acme</code> alongside the <code>dam_global</code> control plane. Application repositories receive a <code>TenantConn</code>, not a general pool handle.',
							'The request opens a transaction and sets <code>search_path</code> for its duration. This makes the safe query the ordinary query: repository SQL does not need a tenant predicate that a future edit can forget.'
						]
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'Counts are disclosures',
						body: 'A scoped caller must never receive a library-wide count beside filtered results. Dashboards, facets and exports use the same access predicate as asset retrieval.'
					}
				]
			},
			{
				id: 'dependency-direction',
				title: 'Dependency direction',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'<code>dam-core</code> contains domain types and depends on no internal crate. Storage, database, media and search build on it. <code>dam-api</code> and <code>dam-mcp</code> sit at the top and do not depend on each other.',
							'That shape keeps policy in shared domain code rather than allowing the REST and agent surfaces to produce different answers.'
						]
					}
				]
			}
		]
	},
	'ingest-delivery': {
		slug: 'ingest-delivery',
		href: '/tour/docs/ingest-delivery',
		nav: 'Ingest and delivery',
		title: 'From upload to governed delivery',
		description:
			'Follow bytes through resumable ingest, verification, derivatives, indexing, rights evaluation and archival restore.',
		category: 'Build with dam.rs',
		readingTime: '10 min',
		keywords: [
			'upload',
			'tus',
			'presign',
			'delivery',
			'rights',
			'derivatives',
			'archive',
			'restore'
		],
		source: {
			label: 'Architecture sections 2, 6 and 10',
			href: 'https://github.com/skippednote/dam.rs/blob/main/ARCHITECTURE.md'
		},
		sections: [
			{
				id: 'one-ingest-path',
				title: 'One ingest path',
				lede: 'Interactive uploads, direct-to-S3 uploads and migrations converge before an asset exists.',
				blocks: [
					{
						kind: 'steps',
						items: [
							{
								title: 'Stage',
								body: 'Create a resumable session and stream chunks, or request a presigned PUT. Bytes remain under the tenant staging prefix.'
							},
							{
								title: 'Finalise',
								body: 'Assemble the object, hash with BLAKE3, sniff the real media type, scan when clamd is configured, then promote by content address.'
							},
							{
								title: 'Derive',
								body: 'Render the thumbnail, preview and web derivative while the original is hot. Preserve colour and provenance evidence.'
							},
							{
								title: 'Index and enrich',
								body: 'Index only after previews exist, then run similarity, colour and any enabled assisted-enrichment jobs.'
							}
						]
					},
					{
						kind: 'callout',
						tone: 'note',
						title: 'Migration does not get a shortcut',
						body: 'damctl import transfer feeds source files into ordinary upload sessions. A migrated asset cannot bypass scanning, derivatives or indexing.'
					}
				]
			},
			{
				id: 'upload-protocols',
				title: 'Choose an upload protocol',
				blocks: [
					{
						kind: 'table',
						headers: ['Path', 'Use it when', 'Required contract'],
						rows: [
							[
								'POST /uploads',
								'The client needs resumability or deferred length',
								'TUS 1.0.0 headers'
							],
							['PATCH /uploads/{id}', 'Sending the next TUS chunk', 'Offset and TUS version'],
							[
								'POST /uploads/presign',
								'The browser can send one direct S3 PUT',
								'Known length and TUS version'
							],
							[
								'damctl import transfer',
								'Moving a library through a connector',
								'JSONL import plan'
							]
						]
					},
					{
						kind: 'code',
						title: 'Open a resumable upload',
						language: 'shell',
						code: [
							"curl -i -X POST 'http://127.0.0.1:8080/uploads' \\",
							"  -H 'Authorization: Bearer $DAMRS_API_KEY' \\",
							"  -H 'Tus-Resumable: 1.0.0' \\",
							"  -H 'Upload-Length: 7340032' \\",
							"  -H 'Upload-Metadata: filename Y2FtcGFpZ24uanBn'"
						].join('\n')
					}
				]
			},
			{
				id: 'delivery-decision',
				title: 'The delivery decision',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'A delivery token binds the tenant, asset or derivative, intended purpose and expiry. Its signature proves the request was minted by dam.rs; it does not freeze the rights verdict.',
							'When <code>GET /d/{token}</code> is redeemed, the server resolves the tenant, checks the caller-independent token constraints, evaluates current rights for the intended use, appends the decision and only then redirects to a short-lived object-store URL.'
						]
					},
					{
						kind: 'list',
						items: [
							'<strong>Internal preview</strong> deliberately remains visible while rights paperwork is unknown.',
							'<strong>Distribution</strong> fails closed when evidence is absent, expired or incompatible with the requested use.',
							'<strong>Archive state</strong> returns a restore estimate instead of minting a URL that object storage will reject.',
							'<strong>Revocation</strong> takes effect on the next fetch because no earlier badge or signature promised future delivery.'
						]
					}
				]
			},
			{
				id: 'storage-tiers',
				title: 'Storage tiers',
				blocks: [
					{
						kind: 'table',
						headers: ['Tier', 'Search and preview', 'Original download'],
						rows: [
							['STANDARD', 'Immediate', 'Immediate'],
							['STANDARD_IA / GLACIER_IR', 'Immediate', 'Immediate with retrieval fee'],
							['GLACIER / DEEP_ARCHIVE', 'Immediate', 'Asynchronous restore']
						]
					},
					{
						kind: 'callout',
						tone: 'success',
						title: 'The proxy is the escape hatch',
						body: 'A generous hot proxy supports visual review and future model upgrades. Re-embedding a Deep Archive library should issue zero restores.'
					}
				]
			}
		]
	},
	'api-mcp': {
		slug: 'api-mcp',
		href: '/tour/docs/api-mcp',
		nav: 'API and MCP',
		title: 'Integrate through REST or MCP',
		description:
			'Use the generated OpenAPI contract for applications and the same authorization model through five focused agent tools.',
		category: 'Build with dam.rs',
		readingTime: '8 min',
		keywords: ['api', 'openapi', 'mcp', 'bearer', 'client', 'contract', 'curl', 'agent'],
		source: {
			label: 'openapi.json',
			href: 'https://github.com/skippednote/dam.rs/blob/main/openapi.json'
		},
		sections: [
			{
				id: 'contract-first',
				title: 'Contract first',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'The checked-in OpenAPI document currently describes <strong>143 paths and 190 operations</strong>. The Rust API generates it, and the Svelte client generates TypeScript types from the same file.',
							'Tests fail when either checked-in layer drifts. That makes the contract executable: a new handler is not complete until clients can discover and type it.'
						]
					},
					{
						kind: 'code',
						title: 'Regenerate the contract and client types',
						language: 'shell',
						code: ['mise run openapi', 'cd web', 'pnpm run gen:api'].join('\n')
					}
				]
			},
			{
				id: 'authentication',
				title: 'Authentication and scope',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Authenticated API calls send a bearer key. A key resolves an identity, tenant and access predicate; handlers do not accept a tenant selector from request data.',
							'Keys are shown once, stored hashed and represented in the interface only by an audit-safe prefix.'
						]
					},
					{
						kind: 'code',
						title: 'List visible assets',
						language: 'shell',
						code: [
							"curl 'http://127.0.0.1:8080/assets?limit=20' \\",
							"  -H 'Authorization: Bearer $DAMRS_API_KEY'"
						].join('\n')
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'Public does not mean unchecked',
						body: 'Delivery, share and portal routes do not require the operator bearer key, but each carries or resolves its own bounded authority. Public delivery still evaluates current rights.'
					}
				]
			},
			{
				id: 'mcp-tools',
				title: 'Five MCP tools',
				lede: 'MCP is another way into the same library, not a parallel authorization system.',
				blocks: [
					{
						kind: 'table',
						headers: ['Tool', 'Purpose'],
						rows: [
							['search_assets', 'Run the same scoped query language as the interface'],
							['get_asset', 'Read metadata and evidence for one visible asset'],
							['get_brand_guidelines', 'Retrieve the tenant guidance needed for use'],
							['check_rights', 'Evaluate a proposed channel and territory'],
							['get_download_url', 'Request a purpose-bound delivery URL']
						]
					},
					{
						kind: 'list',
						items: [
							'Tool discovery reveals shapes, not hidden assets.',
							'Every result is limited by the caller’s compiled access predicate.',
							'<code>get_download_url</code> still enters the ordinary delivery chokepoint.',
							'Cross-tenant asset identifiers return no useful oracle.'
						]
					}
				]
			},
			{
				id: 'errors-and-observability',
				title: 'Errors should teach the contract',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Query parsing refuses an unknown field and reports its character offset instead of silently treating a typo as free text. Readiness names the dependency that failed. Configuration rejects unknown keys at startup.',
							'That directness is part of the API design: a client should be able to correct a request without reading Rust source or inferring state from an empty body.'
						]
					}
				]
			}
		]
	},
	operations: {
		slug: 'operations',
		href: '/tour/docs/operations',
		nav: 'Operations',
		title: 'Deploy and operate dam.rs',
		description:
			'Build the backend image, migrate before rollout, configure dependencies explicitly and monitor the states that require action.',
		category: 'Run it',
		readingTime: '11 min',
		keywords: [
			'deploy',
			'docker',
			'configuration',
			'metrics',
			'health',
			'ready',
			'backup',
			'clamav',
			'kms'
		],
		source: {
			label: 'docker/DEPLOY.md',
			href: 'https://github.com/skippednote/dam.rs/blob/main/docker/DEPLOY.md'
		},
		sections: [
			{
				id: 'artifact-boundary',
				title: 'Know the artifact boundary',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'The backend image carries <code>damd</code>, <code>dam-worker</code>, <code>damctl</code> and the media tools they invoke. The SvelteKit frontend is built and deployed separately.',
							'An image is not a production deployment. TLS termination, a reverse proxy, human authentication, durable search storage, configured virus scanning and an operational backup policy remain the operator’s responsibility.'
						]
					},
					{
						kind: 'code',
						title: 'Build a traceable image',
						language: 'shell',
						code: 'docker build -t damrs:$(git rev-parse --short HEAD) .'
					}
				]
			},
			{
				id: 'deployment-order',
				title: 'Deployment order',
				blocks: [
					{
						kind: 'steps',
						items: [
							{
								title: 'Migrate once',
								body: 'Run damctl migrate --all as a distinct job before new code serves. Do not let every replica race DDL at startup.'
							},
							{
								title: 'Roll the API',
								body: 'Start damd, verify liveness and readiness, then admit traffic.'
							},
							{
								title: 'Roll at least one worker',
								body: 'A healthy API with no worker accepts staging bytes but produces no usable asset.'
							},
							{
								title: 'Verify through the user path',
								body: 'Upload, derive, search and redeem one delivery URL. Probes alone cannot prove the workflow.'
							}
						]
					},
					{
						kind: 'code',
						title: 'Migration job',
						language: 'shell',
						code: 'docker run --rm damrs:TAG damctl migrate --all'
					}
				]
			},
			{
				id: 'configuration',
				title: 'Configuration',
				lede: 'Environment keys use DAMRS_ with a double underscore between sections.',
				blocks: [
					{
						kind: 'table',
						headers: ['Key', 'Why production must set it'],
						rows: [
							['DAMRS_DATABASE__URL', 'The default points to local development'],
							['DAMRS_SERVER__URL_SIGNING_KEY', 'The public placeholder is rejected in production'],
							['DAMRS_STORAGE__BUCKET', 'The default is damrs-dev'],
							['DAMRS_SECURITY__CLAMD_ADDRESS', 'Scanning is off when this is absent'],
							[
								'DAMRS_SERVER__PUBLIC_URL',
								'Absolute links and integrations need the external origin'
							]
						]
					},
					{
						kind: 'code',
						title: 'Inspect the resolved configuration',
						language: 'shell',
						code: 'docker run --rm damrs:TAG damctl config'
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'Unknown configuration is fatal',
						body: 'A misspelled variable does not become a warning followed by a plausible default. Startup refuses it, and damctl config prints the resolved shape with secrets redacted.'
					}
				]
			},
			{
				id: 'probes-and-alerts',
				title: 'Probes and alerts',
				blocks: [
					{
						kind: 'table',
						headers: ['Endpoint or metric', 'Meaning', 'Action'],
						rows: [
							['/health', 'Process liveness', 'Restart a dead or wedged process'],
							['/ready', 'PostgreSQL and object store reachable', 'Remove from traffic on 503'],
							[
								'/metrics',
								'Prometheus output with bearer protection',
								'404 means disabled or wrong token'
							],
							['damrs_jobs{state="dead"}', 'Work exhausted its retries', 'Page an operator'],
							[
								'5xx request class',
								'Handler or dependency failure',
								'Correlate with structured logs'
							]
						]
					},
					{
						kind: 'callout',
						tone: 'note',
						title: 'Readiness deliberately excludes search',
						body: 'Indexes open lazily and are rebuildable. A damaged tenant index should not remove the whole API replica from uploads, downloads and metadata writes.'
					}
				]
			},
			{
				id: 'production-checklist',
				title: 'Production checklist',
				blocks: [
					{
						kind: 'list',
						items: [
							'Terminate TLS and preserve the correct public hostname for signed object-store requests.',
							'Configure clamd and verify a clean, infected and unavailable-scanner path.',
							'Put a seven-day lifecycle safety net under each tenant staging prefix and abort orphaned multipart uploads.',
							'If using customer-managed KMS, configure both API and workers and enforce the key in the bucket policy.',
							'Run a logical backup and restore drill; a completed backup process alone is not recovery proof.',
							'Alert on dead jobs, not merely worker process health.'
						]
					}
				]
			}
		]
	},
	security: {
		slug: 'security',
		href: '/tour/docs/security',
		nav: 'Security model',
		title: 'Security and governance model',
		description:
			'Understand the boundaries worth attacking: tenant isolation, delivery, scoped access, credentials, the audit chain and provenance.',
		category: 'Run it',
		readingTime: '8 min',
		keywords: ['security', 'vulnerability', 'audit', 'provenance', 'credentials', 'tenant', 'c2pa'],
		source: {
			label: 'SECURITY.md',
			href: 'https://github.com/skippednote/dam.rs/blob/main/SECURITY.md'
		},
		sections: [
			{
				id: 'reporting',
				title: 'Report vulnerabilities privately',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Use <a href="https://github.com/skippednote/dam.rs/security/advisories/new" target="_blank" rel="noreferrer">GitHub private vulnerability reporting</a>. Do not open a public issue for a security problem.',
							'A request that demonstrates the issue is more useful than a hypothetical description. Valid reports receive acknowledgement, a note when the fix lands and advisory credit unless the reporter prefers otherwise.'
						]
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'Pre-1.0 support policy',
						body: 'Fixes land on main. There are no maintained release branches yet.'
					}
				]
			},
			{
				id: 'critical-boundaries',
				title: 'Critical boundaries',
				blocks: [
					{
						kind: 'list',
						items: [
							'<strong>Tenant isolation.</strong> Any cross-schema read or write through the application is the highest-severity class of defect.',
							'<strong>Delivery.</strong> Any path that returns bytes without evaluating current rights is in scope even when a signature verifies.',
							'<strong>Access scoping.</strong> Queries, counts, facets and exports must all render the caller’s access predicate.',
							'<strong>Credentials.</strong> API keys, signing secrets, SCIM tokens and share tokens must not be readable back or logged.',
							'<strong>Audit chain.</strong> Database rules refuse update and delete; verification must detect history rewritten outside those rules.',
							'<strong>Provenance.</strong> A tampered file must never be presented as verified.'
						]
					}
				]
			},
			{
				id: 'audit-and-evidence',
				title: 'Audit and evidence',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'Governance actions append to a hash-chained ledger. The database makes ordinary update and delete impossible; <code>damctl audit verify</code> checks the chain later.',
							'This detects superuser tampering rather than pretending a database superuser can be prevented from dropping the rules that protect the table. Detection is the honest boundary.'
						]
					},
					{
						kind: 'code',
						title: 'Verify one tenant’s audit history',
						language: 'shell',
						code: 'damctl audit verify --tenant acme'
					}
				]
			},
			{
				id: 'content-credentials',
				title: 'Content credentials',
				blocks: [
					{
						kind: 'table',
						headers: ['State', 'Meaning'],
						rows: [
							['valid', 'The manifest verifies and chains to a known root'],
							['untrusted', 'The signature verifies but its signer is not recognised'],
							['invalid', 'The binding fails; possible tampering, so evidence is retained'],
							['none', 'No credential is present; this makes no claim about origin']
						]
					},
					{
						kind: 'paragraphs',
						content: [
							'Verification runs on every original without configuration. The manifest is stored under a tier-exempt key, so it remains available after the master moves to Deep Archive.',
							'When derivative signing is configured, rendered files carry a manifest chained to the original. The signing certificate must not be self-signed, the private key must be PKCS#8, and the configured algorithm must match it.'
						]
					}
				]
			},
			{
				id: 'known-limits',
				title: 'Known limits to design around',
				blocks: [
					{
						kind: 'list',
						items: [
							'Human login and SSO are not implemented; operators currently use issued API keys.',
							'Virus scanning is off until clamd is configured, and files above the scan-size ceiling are accepted unscanned with a warning.',
							'Re-scanning existing assets after signature updates is not implemented.',
							'Development credentials and fixtures are intentionally obvious and are not production secrets.'
						]
					},
					{
						kind: 'callout',
						tone: 'note',
						title: 'Read the deployment guide with this page',
						body: 'The security model describes application boundaries. TLS, identity, bucket policy, key policy and backup operations belong to the deployment that surrounds it.'
					}
				]
			}
		]
	},
	contributing: {
		slug: 'contributing',
		href: '/tour/docs/contributing',
		nav: 'Contributing',
		title: 'Contribute with evidence',
		description:
			'The useful unit of change is a small fix with the test that would have caught it and proof through the real user path.',
		category: 'Run it',
		readingTime: '5 min',
		keywords: ['contributing', 'tests', 'lint', 'clippy', 'pull request', 'accessibility', 'ci'],
		source: {
			label: 'CONTRIBUTING.md',
			href: 'https://github.com/skippednote/dam.rs/blob/main/CONTRIBUTING.md'
		},
		sections: [
			{
				id: 'useful-contributions',
				title: 'What is useful now',
				blocks: [
					{
						kind: 'paragraphs',
						content: [
							'dam.rs is young. The highest-value contributions are bug reports with a reproduction and small changes carrying the regression test that would have caught the problem.',
							'Before proposing a broad feature, read <code>TASKS.md</code>, <code>ARCHITECTURE.md</code> and <code>DECISIONS.md</code>. An apparently missing behavior may be a deliberate boundary or a parked decision.'
						]
					},
					{
						kind: 'callout',
						tone: 'warning',
						title: 'Stop at compliance decisions',
						body: 'Changes to rights enforcement, consent, provenance or access control need an already-settled design. Do not turn an ambiguous compliance question into code.'
					}
				]
			},
			{
				id: 'development-loop',
				title: 'Development loop',
				blocks: [
					{
						kind: 'steps',
						items: [
							{
								title: 'Reproduce the failure',
								body: 'Capture the request, state or browser path that proves the current behavior.'
							},
							{
								title: 'Write the failing test',
								body: 'Place it at the lowest layer that still proves the user-visible or contractual property.'
							},
							{
								title: 'Fix the source of the behavior',
								body: 'Avoid a one-off patch when a generator, schema, policy or shared component owns the answer.'
							},
							{
								title: 'Run the real path',
								body: 'A green unit test is necessary. It is not proof that an upload renders, a delivery redirects or a screen can be used.'
							}
						]
					}
				]
			},
			{
				id: 'verification-gates',
				title: 'Verification gates',
				blocks: [
					{
						kind: 'code',
						title: 'Complete local bar',
						language: 'shell',
						code: [
							'mise run check       # Rust formatting, clippy and tests',
							'mise run check:deny  # advisories, licences, bans, sources',
							'mise run check:web   # Svelte, lint, unit, browser and axe',
							'mise run check:all   # all three'
						].join('\n')
					},
					{
						kind: 'paragraphs',
						content: [
							'The web gate includes keyboard behavior, virtual-grid semantics, both themes and WCAG 2.1 AA scans. The generated API types are checked against the current OpenAPI document.',
							'The AWS archival gate is separate because it spends real money and waits on real restore behavior. Point it only at a throwaway bucket.'
						]
					}
				]
			},
			{
				id: 'pull-request-shape',
				title: 'Pull request shape',
				blocks: [
					{
						kind: 'list',
						items: [
							'Keep one logical change together and unrelated cleanup out.',
							'Explain the broken property, the root cause and the evidence that now proves it.',
							'Call out migrations, wire-contract changes and operational consequences explicitly.',
							'Include screenshots for interface work and commands for behavior that cannot be seen in a diff.',
							'Do not claim the whole deployment from a local check; name the boundary of the proof.'
						]
					}
				]
			}
		]
	}
};

export function isDocSlug(value: string): value is DocSlug {
	return value in DOCS;
}
