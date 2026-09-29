import { spawn } from 'node:child_process';
import { cp, mkdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';

const output = path.resolve('pages-dist');
const origin = 'http://127.0.0.1:4175';
const pagesBase = '/dam.rs';
const publicOrigin = 'https://skippednote.github.io/dam.rs';
const routes = [
	['/tour/', `${pagesBase}/tour`],
	['/tour/docs/', `${pagesBase}/tour/docs`],
	['/tour/docs/architecture/', `${pagesBase}/tour/docs/architecture`],
	['/tour/docs/ingest-delivery/', `${pagesBase}/tour/docs/ingest-delivery`],
	['/tour/docs/api-mcp/', `${pagesBase}/tour/docs/api-mcp`],
	['/tour/docs/operations/', `${pagesBase}/tour/docs/operations`],
	['/tour/docs/security/', `${pagesBase}/tour/docs/security`],
	['/tour/docs/contributing/', `${pagesBase}/tour/docs/contributing`]
];

await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
await cp(path.resolve('build/client', pagesBase.slice(1)), output, { recursive: true });

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
			const response = await fetch(`${origin}${pagesBase}/tour`);
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
	const html = await response.text();
	if (!html.includes(`${pagesBase}/_app/`)) {
		throw new Error(`GET ${source} did not use the GitHub Pages asset base`);
	}
	await writeFile(path.join(directory, 'index.html'), html);
}

function sitemap() {
	const urls = [`${publicOrigin}/`, ...routes.map(([route]) => `${publicOrigin}${route}`)];
	return [
		'<?xml version="1.0" encoding="UTF-8"?>',
		'<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
		...urls.map((url) => `  <url><loc>${url}</loc></url>`),
		'</urlset>',
		''
	].join('\n');
}

function rootRedirect() {
	const target = `${pagesBase}/tour/`;
	return `<!doctype html>
<html lang="en">
	<head>
		<meta charset="utf-8">
		<meta name="viewport" content="width=device-width, initial-scale=1">
		<meta http-equiv="refresh" content="0; url=${target}">
		<link rel="canonical" href="${publicOrigin}/tour/">
		<title>dam.rs · Rights-aware digital asset management</title>
		<script>location.replace("${target}")</script>
	</head>
	<body>
		<p><a href="${target}">Open dam.rs</a></p>
	</body>
</html>
`;
}

try {
	await waitUntilReady();
	for (const [target, source] of routes) {
		await render(target, source);
	}

	await writeFile(path.join(output, 'index.html'), rootRedirect());
	const notFound = await fetch(`${origin}${pagesBase}/this-page-does-not-exist`);
	await writeFile(path.join(output, '404.html'), await notFound.text());
	await writeFile(path.join(output, '.nojekyll'), '');
	await writeFile(path.join(output, 'sitemap.xml'), sitemap());
	console.log(`Exported ${routes.length} pages and the root redirect to ${output}`);
} finally {
	server.kill('SIGTERM');
}
