import { spawn } from 'node:child_process';
import { cp, mkdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';

const output = path.resolve('pages-dist');
const origin = 'http://127.0.0.1:4175';
const routes = [
	['/', '/tour'],
	['/tour/', '/tour'],
	['/tour/docs/', '/tour/docs'],
	['/tour/docs/architecture/', '/tour/docs/architecture'],
	['/tour/docs/ingest-delivery/', '/tour/docs/ingest-delivery'],
	['/tour/docs/api-mcp/', '/tour/docs/api-mcp'],
	['/tour/docs/operations/', '/tour/docs/operations'],
	['/tour/docs/security/', '/tour/docs/security'],
	['/tour/docs/contributing/', '/tour/docs/contributing']
];

await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
await cp(path.resolve('build/client'), output, { recursive: true });

const server = spawn(process.execPath, ['build/index.js'], {
	env: { ...process.env, HOST: '127.0.0.1', PORT: '4175', NODE_ENV: 'production' },
	stdio: ['ignore', 'pipe', 'pipe']
});

let serverOutput = '';
server.stdout.on('data', (chunk) => (serverOutput += chunk));
server.stderr.on('data', (chunk) => (serverOutput += chunk));

async function waitUntilReady() {
	for (let attempt = 0; attempt < 80; attempt += 1) {
		try {
			const response = await fetch(`${origin}/tour`);
			if (response.ok) return;
		} catch {
			// The server has not bound its socket yet.
		}
		await new Promise((resolve) => setTimeout(resolve, 100));
	}
	throw new Error(`the production server did not become ready\n${serverOutput}`);
}

async function render(target, source) {
	const response = await fetch(`${origin}${source}`);
	if (!response.ok) {
		throw new Error(`GET ${source} returned ${response.status}`);
	}

	const directory = target === '/' ? output : path.join(output, target);
	await mkdir(directory, { recursive: true });
	await writeFile(path.join(directory, 'index.html'), normalizeAssetPaths(await response.text()));
}

function normalizeAssetPaths(html) {
	// SvelteKit emits paths relative to the route it rendered. GitHub Pages serves directory indexes
	// with a trailing slash, which adds one path segment and makes ../../_app resolve to /tour/_app.
	// Root-absolute references are correct because this is a user site at damrs.github.io, not a project
	// site under a repository subdirectory.
	return html.replaceAll(/(?:\.\.?\/)*\/?_app\//g, '/_app/');
}

function sitemap() {
	const urls = routes.map(([route]) =>
		route === '/' ? 'https://damrs.github.io/' : `https://damrs.github.io${route}`
	);
	return [
		'<?xml version="1.0" encoding="UTF-8"?>',
		'<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
		...urls.map((url) => `  <url><loc>${url}</loc></url>`),
		'</urlset>',
		''
	].join('\n');
}

try {
	await waitUntilReady();
	for (const [target, source] of routes) {
		await render(target, source);
	}

	const notFound = await fetch(`${origin}/this-page-does-not-exist`);
	await writeFile(path.join(output, '404.html'), normalizeAssetPaths(await notFound.text()));
	await writeFile(path.join(output, '.nojekyll'), '');
	await writeFile(path.join(output, 'sitemap.xml'), sitemap());
	console.log(`Exported ${routes.length} pages to ${output}`);
} finally {
	server.kill('SIGTERM');
}
