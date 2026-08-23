<script lang="ts">
	/**
	 * Connected sites: what renders this library's images, and what each one may have.
	 *
	 * ## Two credentials, shown once, and the page says which is which
	 *
	 * The API key is how the site calls damrs. The signing secret is how it signs render URLs *itself*, so a
	 * page render never blocks on an API call — which makes the secret a forgery capability for whatever that
	 * site may render. Neither can be read back: the key is stored as a hash and the secret encrypted at rest.
	 * So they appear once, in a panel that stays until dismissed, with the sentence saying so. A UI that showed
	 * them in a toast would produce a support ticket a week later.
	 *
	 * ## Rotation asks which situation it is, because the answers are opposite
	 *
	 * A scheduled rotation keeps the old secret working for a week while the site's own deploy lands — a
	 * rotation with no window is an outage. A leak must stop it now, because that week would be a week of
	 * forgery. Two buttons rather than a checkbox with a default: a default is wrong half the time, in the
	 * direction that matters.
	 *
	 * ## What a site may have is stated per row, not buried in a form
	 *
	 * `allow_original` and `allow_restore` are both off by default and both are consequential — one is whether
	 * a website can fetch the master a customer paid for, the other whether a page render can wake Glacier. The
	 * row says which, in words rather than as two unlabelled ticks.
	 *
	 * ## Revoking is terminal and the confirmation says so
	 *
	 * The secret is already out there, so reactivating would bring back every URL the site ever signed. Pausing
	 * is the reversible one, and the difference is the whole reason both exist.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import {
		ApiError,
		listConnectors,
		registerConnector,
		rotateConnector,
		setConnectorStatus,
		type Connector,
		type Registered
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	/** The kinds the server accepts. Drupal first: it is the one §11 is built for. */
	const KINDS = [
		{ value: 'drupal', label: 'Drupal' },
		{ value: 'wordpress', label: 'WordPress' },
		{ value: 'adobe_cc', label: 'Adobe CC' },
		{ value: 'figma', label: 'Figma' },
		{ value: 'hubspot', label: 'HubSpot' },
		{ value: 'salesforce', label: 'Salesforce' },
		{ value: 'generic', label: 'Something else' }
	];

	let sites = $state<Connector[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');
	let busy = $state('');

	/** The one-time credentials panel. Null until something mints a pair. */
	let issued = $state<{ registered: Registered; label: string } | null>(null);

	let formOpen = $state(false);
	let kind = $state('drupal');
	let label = $state('');
	let siteUrl = $state('');
	let allGroups = $state(false);
	let allowOriginal = $state(false);
	let allowRestore = $state(false);

	/** Which row is asking about revocation. Inline, not a `confirm()`, which blocks the automation. */
	let revoking = $state('');

	async function load() {
		try {
			sites = await listConnectors();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Connected sites are administration; this key does not hold Manage.'
					: caught instanceof ApiError
						? caught.message
						: 'Could not read the connected sites.';
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		if (!session.connected) {
			loading = false;
			return;
		}
		void load();
	});

	async function submit() {
		error = '';
		notice = '';
		busy = 'register';
		try {
			const registered = await registerConnector({
				kind,
				label: label.trim(),
				site_url: siteUrl.trim(),
				all_asset_groups: allGroups,
				allow_original: allowOriginal,
				allow_restore: allowRestore
			});
			issued = { registered, label: registered.connector.label };
			formOpen = false;
			label = '';
			siteUrl = '';
			allGroups = false;
			allowOriginal = false;
			allowRestore = false;
			sites = await listConnectors();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'That site could not be connected.';
		} finally {
			busy = '';
		}
	}

	async function rotate(site: Connector, keepPrevious: boolean) {
		error = '';
		notice = '';
		busy = site.id;
		try {
			const registered = await rotateConnector(site.id, keepPrevious);
			issued = { registered, label: site.label };
			notice = keepPrevious
				? `New secret issued. The previous one keeps working for a week, so ${site.label} has time to deploy.`
				: `New secret issued and the previous one is dead. ${site.label} will render nothing until it deploys.`;
			sites = await listConnectors();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not rotate that secret.';
		} finally {
			busy = '';
		}
	}

	async function change(site: Connector, status: 'active' | 'paused' | 'revoked') {
		error = '';
		notice = '';
		busy = site.id;
		try {
			await setConnectorStatus(site.id, status);
			revoking = '';
			notice =
				status === 'revoked'
					? `${site.label} is revoked. Its secrets are gone and its URLs stopped working.`
					: status === 'paused'
						? `${site.label} is paused. Its images stopped rendering.`
						: `${site.label} is active again.`;
			sites = await listConnectors();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not change that.';
		} finally {
			busy = '';
		}
	}

	function stamp(at: string | null | undefined): string {
		return at ? new Date(at).toLocaleString() : 'never';
	}
</script>

<svelte:head><title>Connected sites · damrs</title></svelte:head>

<div class="mx-auto max-w-4xl space-y-6 p-8">
	<header class="space-y-1">
		<h1 class="text-2xl font-semibold tracking-tight">Connected sites</h1>
		<p class="max-w-2xl text-sm text-muted">
			A connected site stores an asset id and renders signed URLs — it never keeps a copy of the
			file. That is what makes rights real downstream: when a licence expires here, the image stops
			rendering there. Each site is scoped to asset groups, so it can only ever show what you have
			given it.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>
	{#if notice}
		<p class="text-xs text-muted">{notice}</p>
	{/if}

	{#if issued}
		<!--
			Stays until dismissed, deliberately.

			Neither value can be read back — the key is stored as a hash and the secret encrypted at rest — so
			this panel is the only place either exists. A toast would be the wrong shape for something somebody
			has to copy into another system's configuration.
		-->
		<section
			class="space-y-3 rounded-md border border-state-rights-expiring p-3"
			aria-labelledby="issued"
			data-testid="issued"
		>
			<h2 id="issued" class="text-sm font-semibold tracking-tight">
				Credentials for {issued.label}
			</h2>
			<p class="max-w-2xl text-xs text-muted">{issued.registered.warning}</p>
			{#if issued.registered.api_key}
				<label class="block space-y-1">
					<span class="text-xs text-muted">API key — how the site calls this library</span>
					<input
						readonly
						class="w-full rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs"
						value={issued.registered.api_key}
						data-testid="issued-key"
					/>
				</label>
			{/if}
			<label class="block space-y-1">
				<span class="text-xs text-muted">
					Signing secret — how the site signs its own image URLs, so its pages keep rendering even
					if this library is unreachable
				</span>
				<input
					readonly
					class="w-full rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs"
					value={issued.registered.signing_secret}
					data-testid="issued-secret"
				/>
			</label>
			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 text-sm hover:bg-raised"
				onclick={() => (issued = null)}
			>
				I have copied both
			</button>
		</section>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else}
		<div>
			<button
				type="button"
				class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg"
				onclick={() => (formOpen = !formOpen)}
				aria-expanded={formOpen}
				data-testid="connect"
			>
				Connect a site…
			</button>
		</div>

		{#if formOpen}
			<form
				class="space-y-3 rounded-md border border-line p-3"
				onsubmit={(event) => {
					event.preventDefault();
					void submit();
				}}
			>
				<div class="flex flex-wrap gap-3">
					<label class="space-y-1">
						<span class="block text-xs text-muted">What is it</span>
						<select
							bind:value={kind}
							class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
							data-testid="kind"
						>
							{#each KINDS as option (option.value)}
								<option value={option.value}>{option.label}</option>
							{/each}
						</select>
					</label>
					<label class="space-y-1">
						<span class="block text-xs text-muted">Name it</span>
						<input
							bind:value={label}
							placeholder="Marketing site"
							class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
							data-testid="label"
						/>
					</label>
					<label class="min-w-64 flex-1 space-y-1">
						<span class="block text-xs text-muted">
							Its address — used to allow its browser requests, so it has to be the origin the site
							actually serves from
						</span>
						<input
							bind:value={siteUrl}
							placeholder="https://www.example.com"
							class="w-full rounded-md border border-line bg-bg px-2 py-1 text-sm"
							data-testid="site-url"
						/>
					</label>
				</div>

				<fieldset class="space-y-1">
					<legend class="text-xs text-muted">What it may have</legend>
					<!--
						Three separate consequences, said in words. Two unlabelled ticks marked "original" and
						"restore" would be the version of this form where somebody grants master access to a
						public website by accident.
					-->
					<label class="flex items-start gap-2 text-sm">
						<input type="checkbox" bind:checked={allGroups} class="mt-1 rounded border-line" />
						<span>
							Everything in the library.
							<span class="text-xs text-muted">
								Leave this off and grant it specific asset groups instead — a site that can only see
								what it needs cannot publish what it should not.
							</span>
						</span>
					</label>
					<label class="flex items-start gap-2 text-sm">
						<input type="checkbox" bind:checked={allowOriginal} class="mt-1 rounded border-line" />
						<span>
							May fetch original files.
							<span class="text-xs text-muted">
								Usually not: a website wants web-sized renditions, and a site that can fetch masters
								is a site that can leak the deliverable a customer paid for.
							</span>
						</span>
					</label>
					<label class="flex items-start gap-2 text-sm">
						<input type="checkbox" bind:checked={allowRestore} class="mt-1 rounded border-line" />
						<span>
							May pull files out of cold storage.
							<span class="text-xs text-muted">
								Almost never. A page render would trigger a retrieval nobody asked for and somebody
								pays for. With this off, a cold original renders as the web-sized copy instead.
							</span>
						</span>
					</label>
				</fieldset>

				<button
					type="submit"
					class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
					disabled={!label.trim() || !siteUrl.trim() || busy === 'register'}
					data-testid="submit"
				>
					Connect it
				</button>
			</form>
		{/if}

		{#if sites.length === 0}
			<p class="max-w-2xl text-sm text-muted">
				Nothing is connected. A connected site renders this library's images on its own pages
				without copying the files, which is what makes an expiring licence take effect there as well
				as here.
			</p>
		{:else}
			<ul class="space-y-3" data-testid="sites">
				{#each sites as site (site.id)}
					<li class="space-y-2 rounded-md border border-line p-3">
						<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1">
							<h2 class="text-sm font-semibold tracking-tight">{site.label}</h2>
							<span
								class="rounded border px-1.5 py-0.5 text-xs {site.status === 'active'
									? 'border-state-rights-allowed text-state-rights-allowed-fg'
									: site.status === 'error'
										? 'border-state-rights-expiring text-state-rights-expiring-fg'
										: 'border-line text-muted'}"
								data-testid="status-{site.id}"
							>
								{site.status}
							</span>
							{#if !site.may_render && site.status !== 'revoked'}
								<span class="text-xs text-muted">its images are not rendering</span>
							{/if}
							<span class="text-xs text-muted">{site.kind}</span>
							<!--
								`rel="external"` because it is: this is the customer's own website, not a route in
								this application. It tells SvelteKit not to intercept the navigation, and it is
								what `no-navigation-without-resolve` looks for — `resolve()` is for internal
								routes and would be meaningless here.
							-->
							<a
								href={site.site_url}
								rel="external noreferrer"
								class="text-xs underline decoration-line hover:decoration-fg">{site.site_url}</a
							>
						</div>

						<p class="flex flex-wrap gap-x-3 gap-y-1 text-xs text-muted">
							<span>
								{site.all_asset_groups
									? 'sees the whole library'
									: site.asset_group_ids.length === 0
										? 'sees nothing yet — grant it an asset group'
										: `${site.asset_group_ids.length} asset ${site.asset_group_ids.length === 1 ? 'group' : 'groups'}`}
							</span>
							<span>{site.allow_original ? 'may fetch originals' : 'renditions only'}</span>
							{#if site.allow_restore}<span>may pull from cold storage</span>{/if}
							<span>last called {stamp(site.last_seen_at)}</span>
							{#if site.remote_version}<span>{site.remote_version}</span>{/if}
						</p>

						{#if site.last_error}
							<p class="text-xs text-state-rights-expiring-fg">
								Last problem: {site.last_error}
							</p>
						{/if}

						{#if site.previous_secret_live}
							<!-- The state somebody needs to act on: two secrets are valid, and one is about to
							     stop being. -->
							<p class="text-xs text-muted" data-testid="grace-{site.id}">
								Rotated {stamp(site.secret_rotated_at)}. The previous secret still works, so this
								site keeps rendering until it deploys the new one.
							</p>
						{/if}

						{#if site.status !== 'revoked'}
							<div class="flex flex-wrap gap-2 text-sm">
								<button
									type="button"
									class="rounded-md border border-line px-2.5 py-1 hover:bg-raised disabled:opacity-50"
									disabled={busy === site.id}
									onclick={() => rotate(site, true)}
									data-testid="rotate-{site.id}"
								>
									New secret, keep the old one a week
								</button>
								<button
									type="button"
									class="rounded-md border border-state-rights-expiring px-2.5 py-1 text-state-rights-expiring-fg disabled:opacity-50"
									disabled={busy === site.id}
									onclick={() => rotate(site, false)}
									data-testid="rotate-now-{site.id}"
								>
									It leaked — replace it now
								</button>
								{#if site.status === 'paused'}
									<button
										type="button"
										class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
										disabled={busy === site.id}
										onclick={() => change(site, 'active')}
									>
										Resume
									</button>
								{:else}
									<button
										type="button"
										class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
										disabled={busy === site.id}
										onclick={() => change(site, 'paused')}
										data-testid="pause-{site.id}"
									>
										Pause
									</button>
								{/if}
								{#if revoking === site.id}
									<span class="flex flex-wrap items-center gap-2 text-xs">
										<span class="max-w-md">
											Revoking is permanent. Both secrets are destroyed and nothing brings this site
											back — pause it instead if you might want it later.
										</span>
										<button
											type="button"
											class="rounded-md border border-state-rights-denied px-2.5 py-1 text-sm text-state-rights-denied-fg"
											disabled={busy === site.id}
											onclick={() => change(site, 'revoked')}
											data-testid="revoke-confirm-{site.id}"
										>
											Revoke permanently
										</button>
										<button
											type="button"
											class="rounded-md border border-line px-2.5 py-1 text-sm hover:bg-raised"
											onclick={() => (revoking = '')}
										>
											Keep it
										</button>
									</span>
								{:else}
									<button
										type="button"
										class="text-xs text-muted underline hover:text-fg"
										onclick={() => (revoking = site.id)}
										data-testid="revoke-{site.id}"
									>
										Revoke
									</button>
								{/if}
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</div>
