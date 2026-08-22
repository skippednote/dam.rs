<script lang="ts">
	/**
	 * The connection screen.
	 *
	 * An API key rather than a login, because that is what the backend has: `dam_global.api_keys` with a
	 * hashed secret and a scope list. A session endpoint issuing a short-lived `HttpOnly` cookie is the
	 * right long-term answer and it is a backend change, so this is honest about being the interim: the
	 * key is stored where script can read it, the exposure is stated, and clearing it is one click.
	 *
	 * The connection is *checked* rather than assumed. A key typed with a missing character produces a 401
	 * on the first grid load, several screens away from where it was entered — so this page proves the key
	 * works before letting the user leave it.
	 */
	import { resolve } from '$app/paths';
	import { health, listAssets, ApiError } from '$lib/api/client';
	import { DEFAULT_BASE, session } from '$lib/api/session.svelte';

	let base = $state(session.base || DEFAULT_BASE);
	let key = $state('');
	let checking = $state(false);
	let result = $state<{ ok: boolean; message: string } | null>(null);

	async function connect(event: SubmitEvent) {
		event.preventDefault();
		checking = true;
		result = null;

		// The server first, so "wrong port" and "wrong key" are told apart. Without it both look like a
		// rejected credential and somebody re-issues a key that was fine.
		if (!(await health(base))) {
			result = {
				ok: false,
				message: `No server answered at ${base}. Is damd running, and is the port right?`
			};
			checking = false;
			return;
		}

		session.connect(key || session.key, base);
		try {
			// A real authenticated request, not a health check: only this proves the key is accepted, that
			// its identity has a membership, and that the tenant's schema exists.
			const page = await listAssets({ limit: 1 });
			result = {
				ok: true,
				message: `Connected. ${page.total} asset${page.total === 1 ? '' : 's'} visible to this key.`
			};
			key = '';
		} catch (caught) {
			const message =
				caught instanceof ApiError
					? caught.status === 403
						? 'The key was accepted but grants nothing. Was it issued with --admin, or does its identity hold a role?'
						: caught.message
					: 'Could not reach the API.';
			result = { ok: false, message };
			// Not kept on failure: a stored key that does not work produces a 401 on every screen and looks
			// like the app is broken rather than unconfigured.
			session.disconnect();
		} finally {
			checking = false;
		}
	}
</script>

<div class="mx-auto max-w-xl space-y-6 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">Connection</h1>
		<p class="mt-1 text-sm text-muted">
			Issue a key with
			<code class="rounded bg-surface px-1 font-mono text-xs"
				>damctl issue-key --tenant &lt;slug&gt; --email you@example.com --admin</code
			>. It is printed once and only its hash is stored, so it cannot be recovered afterwards.
		</p>
	</div>

	<nav aria-label="Settings" class="flex gap-3 text-sm">
		<span aria-current="page" class="rounded-md bg-surface px-2.5 py-1 font-medium">Connection</span
		>
		<a class="rounded-md px-2.5 py-1 text-muted hover:text-fg" href={resolve('/settings/ai')}>
			AI models
		</a>
	</nav>

	<form class="space-y-4" onsubmit={connect}>
		<div class="space-y-1">
			<label class="block text-xs font-semibold tracking-wide text-muted uppercase" for="base">
				API address
			</label>
			<input
				id="base"
				class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
				bind:value={base}
				placeholder={DEFAULT_BASE}
			/>
		</div>

		<div class="space-y-1">
			<label class="block text-xs font-semibold tracking-wide text-muted uppercase" for="key">
				API key
			</label>
			<input
				id="key"
				type="password"
				autocomplete="off"
				class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
				bind:value={key}
				placeholder={session.connected ? 'Stored — leave blank to keep it' : 'damrs_…'}
				aria-describedby="key-note"
			/>
			<p id="key-note" class="text-xs text-muted">
				Stored in this browser's local storage, which script on this origin can read. There is no
				session endpoint yet; when there is, this becomes a short-lived cookie.
			</p>
		</div>

		<div class="flex items-center gap-3">
			<button
				type="submit"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
				disabled={checking || (!key && !session.connected)}
			>
				{checking ? 'Checking…' : 'Connect'}
			</button>
			{#if session.connected}
				<span class="font-mono text-xs text-muted">{session.visible}</span>
				<button
					type="button"
					class="text-xs underline"
					onclick={() => {
						session.disconnect();
						result = null;
					}}
				>
					Forget this key
				</button>
			{/if}
		</div>
	</form>

	{#if result}
		<!--
			`role="status"` rather than `alert` even for a failure: the user submitted a form and is waiting
			for this, so it does not need to interrupt. `aria-live` on a container that already exists would
			not announce; this one is created by the update, which is why it carries the role itself.
		-->
		<p
			role="status"
			class="rounded-md p-3 text-sm {result.ok
				? 'bg-state-rights-allowed/18 text-state-rights-allowed-fg'
				: 'bg-state-rights-denied/18 text-state-rights-denied-fg'}"
		>
			{result.message}
		</p>
	{/if}
</div>
