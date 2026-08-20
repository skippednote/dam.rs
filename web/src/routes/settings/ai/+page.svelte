<script lang="ts">
	/**
	 * Hosted-model configuration: a tenant's own provider keys, and the cap on what enrichment may spend.
	 *
	 * ## A key is typed once and never shown again
	 *
	 * The API has no route that returns a key — sealed or otherwise — so this screen cannot show one either.
	 * What it shows is a four-character hint, which is enough to tell two keys apart and useless to anybody
	 * reading over a shoulder. The input is cleared the moment the request resolves, and rotation is a
	 * *replacement* rather than an edit for the same reason.
	 *
	 * ## Verification is a real call
	 *
	 * Everything else about the integration is tested against recorded fixtures, so "the key is stored" and
	 * "the key works" are different facts. The button asks the provider one short question and reports which
	 * of three things happened: it worked, the credential was rejected, or the model declined — the last of
	 * which means the credential is fine.
	 *
	 * ## The budget is in dollars here and cents on the wire
	 *
	 * Nobody types a cap in cents. The conversion happens at the edge, once, and the read-back is what the
	 * server stored rather than what was typed.
	 */
	import {
		addAiCredential,
		readEnrichmentSettings,
		saveEnrichmentSettings,
		listAiCredentials,
		makeAiCredentialDefault,
		readAiBudget,
		replaceAiCredentialKey,
		setAiBudget,
		setAiCredentialActive,
		verifyAiCredential,
		ApiError,
		type AiBudget,
		type AiCredential,
		type AiVerifyResult,
		type EnrichmentSettings
	} from '$lib/api/client';
	import { resolve } from '$app/paths';
	import { session } from '$lib/api/session.svelte';
	import { onMount } from 'svelte';

	let credentials = $state<AiCredential[]>([]);
	let budget = $state<AiBudget | null>(null);
	let problem = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let verified = $state<Record<string, AiVerifyResult>>({});

	// The add form.
	let provider = $state<'anthropic' | 'openai_compatible'>('anthropic');
	let label = $state('');
	let baseUrl = $state('');
	let defaultModel = $state('claude-opus-5');
	let apiKey = $state('');
	let makeDefault = $state(true);

	// Rotation, one row at a time: a single open field, so a key cannot be pasted into the wrong row's box.
	let rotating = $state<string | null>(null);
	let rotationKey = $state('');

	// A cap in dollars, because that is the unit somebody thinks in.
	let limitDollars = $state('');
	let hard = $state(true);

	// What a model should do, once there is a key for it to do it with.
	let enrichment = $state<EnrichmentSettings | null>(null);

	const endpointNeeded = $derived(provider === 'openai_compatible');

	async function load() {
		problem = null;
		try {
			[credentials, budget, enrichment] = await Promise.all([
				listAiCredentials(),
				readAiBudget(),
				readEnrichmentSettings()
			]);
			// `== null` on purpose: the generated type is optional *and* nullable, and both mean "no cap".
			limitDollars = budget.limit_cents == null ? '' : (budget.limit_cents / 100).toFixed(2);
			// Only mirror the stored enforcement when there is something stored. With no cap configured the
			// server reports `soft` as a placeholder, and copying that into the form would offer an unmetered
			// tenant the *less* prudent default — the schema's own note says a hard cap on AI enrichment is the
			// sensible one, since the failure it prevents is a surprise invoice rather than lost work.
			if (budget.limit_cents != null) {
				hard = budget.enforcement === 'hard';
			}
		} catch (caught) {
			problem = describe(caught);
		}
	}

	function describe(caught: unknown): string {
		if (caught instanceof ApiError) {
			return caught.status === 403
				? 'This key may not change tenant configuration. Model credentials need manage access.'
				: caught.message;
		}
		return 'Could not reach the API.';
	}

	async function act<T>(id: string, work: () => Promise<T>, said: (result: T) => string) {
		busy = id;
		problem = null;
		notice = null;
		try {
			notice = said(await work());
			credentials = await listAiCredentials();
		} catch (caught) {
			problem = describe(caught);
		} finally {
			busy = null;
		}
	}

	async function add(event: SubmitEvent) {
		event.preventDefault();
		await act(
			'add',
			() =>
				addAiCredential({
					provider,
					label,
					// Empty means absent. Sending "" would store an endpoint that resolves to nothing.
					base_url: baseUrl.trim() === '' ? null : baseUrl.trim(),
					default_model: defaultModel,
					api_key: apiKey,
					make_default: makeDefault
				}),
			(created) => `Stored ${created.label}, ending ${created.hint}.`
		);
		// Cleared whatever happened: on success it is stored, and on failure a key left in a field outlives
		// the mistake that put it there.
		apiKey = '';
		label = '';
	}

	async function rotate(id: string) {
		const key = rotationKey;
		rotationKey = '';
		await act(
			id,
			() => replaceAiCredentialKey(id, key),
			(updated) => `Replaced the key for ${updated.label}; it now ends ${updated.hint}.`
		);
		rotating = null;
	}

	async function verify(id: string) {
		busy = id;
		problem = null;
		notice = null;
		try {
			verified = { ...verified, [id]: await verifyAiCredential(id) };
		} catch (caught) {
			problem = describe(caught);
		} finally {
			busy = null;
		}
	}

	async function saveBudget(event: SubmitEvent) {
		event.preventDefault();
		busy = 'budget';
		problem = null;
		notice = null;
		try {
			budget = await setAiBudget({
				limit_cents: Math.round(Number(limitDollars || '0') * 100),
				hard
			});
			notice = 'Spend cap saved.';
		} catch (caught) {
			problem = describe(caught);
		} finally {
			busy = null;
		}
	}

	async function saveEnrichment(event: SubmitEvent) {
		event.preventDefault();
		if (!enrichment) return;
		busy = 'enrichment';
		problem = null;
		notice = null;
		try {
			enrichment = await saveEnrichmentSettings(enrichment);
			notice = enrichment.is_enabled
				? 'Enrichment is on. New uploads will be described.'
				: 'Enrichment is off. Nothing will be described.';
		} catch (caught) {
			problem = describe(caught);
			// Re-read rather than leaving the form showing a state the server refused — the commonest refusal
			// here is "turn it on without a key", and the checkbox must not stay ticked afterwards.
			try {
				enrichment = await readEnrichmentSettings();
			} catch {
				enrichment = null;
			}
		} finally {
			busy = null;
		}
	}

	function money(cents: number): string {
		return `$${(cents / 100).toFixed(2)}`;
	}

	onMount(() => {
		if (session.connected) void load();
	});
</script>

<div class="mx-auto max-w-3xl space-y-8 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">AI models</h1>
		<p class="mt-1 text-sm text-muted">
			Bring your own key. Anthropic speaks its own format; everything else — ChatGPT, Kimi,
			DeepSeek, Together, Groq, a local server — speaks the OpenAI-compatible one, so a vendor here
			is an endpoint and a model name.
		</p>
	</div>

	{#if !session.connected}
		<p class="rounded-md bg-surface p-3 text-sm">
			Not connected. Add an API key on the
			<a class="underline" href={resolve('/settings')}>connection</a> screen first.
		</p>
	{:else}
		{#if problem}
			<p
				role="alert"
				class="rounded-md bg-state-rights-denied/18 p-3 text-sm text-state-rights-denied-fg"
			>
				{problem}
			</p>
		{/if}
		{#if notice}
			<p
				role="status"
				class="rounded-md bg-state-rights-allowed/18 p-3 text-sm text-state-rights-allowed-fg"
			>
				{notice}
			</p>
		{/if}

		<section class="space-y-3" aria-labelledby="budget-heading">
			<h2 id="budget-heading" class="text-lg font-medium">Spend cap</h2>
			{#if budget}
				<p class="text-sm text-muted">
					{money(budget.used_cents)} used this month{budget.limit_cents == null
						? ', with no cap set — enrichment runs unmetered.'
						: ` of ${money(budget.limit_cents)}.`}
					{#if budget.state === 'refused'}
						<span class="font-medium text-state-rights-denied-fg">
							Over the cap: new enrichment is refused.
						</span>
					{:else if budget.state === 'warned'}
						<span class="font-medium">Past the warning line.</span>
					{/if}
				</p>
			{/if}
			<form class="flex flex-wrap items-end gap-3" onsubmit={saveBudget}>
				<div class="space-y-1">
					<label class="block text-xs font-semibold tracking-wide text-muted uppercase" for="limit">
						Monthly limit (US dollars)
					</label>
					<input
						id="limit"
						class="w-40 rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
						bind:value={limitDollars}
						inputmode="decimal"
						placeholder="0.00"
					/>
				</div>
				<label class="flex items-center gap-2 text-sm">
					<input type="checkbox" bind:checked={hard} />
					Refuse enrichment past the limit
				</label>
				<button
					type="submit"
					class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
					disabled={busy === 'budget'}
				>
					{busy === 'budget' ? 'Saving…' : 'Save cap'}
				</button>
			</form>
			<p class="text-xs text-muted">
				Unticked, the cap warns and keeps working. A cap can be passed by the calls already in
				flight when the limit is crossed — the cost of a call is only known once it has been made.
			</p>
		</section>

		<section class="space-y-3" aria-labelledby="enrichment-heading">
			<h2 id="enrichment-heading" class="text-lg font-medium">Description and tags</h2>
			{#if enrichment}
				<form class="space-y-4" onsubmit={saveEnrichment}>
					<label class="flex items-start gap-2 text-sm">
						<input type="checkbox" bind:checked={enrichment.is_enabled} class="mt-0.5" />
						<span>
							Describe new uploads
							<span class="block text-xs text-muted">
								Off until you turn it on, because each asset costs a fraction of a cent and a large
								library adds up. A person reviews every tag before it counts.
							</span>
						</span>
					</label>

					<div class="space-y-1">
						<label
							class="block text-xs font-semibold tracking-wide text-muted uppercase"
							for="guidance"
						>
							House guidance
						</label>
						<textarea
							id="guidance"
							rows="3"
							class="w-full rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
							bind:value={enrichment.guidance}
							placeholder="Say 'trainers', not 'sneakers'. Never guess a location."
							aria-describedby="guidance-note"></textarea>
						<p id="guidance-note" class="text-xs text-muted">
							Sent with every request and cached by the provider, so it is nearly free to be
							specific here.
						</p>
					</div>

					<div class="grid gap-4 sm:grid-cols-2">
						<div class="space-y-1">
							<label
								class="block text-xs font-semibold tracking-wide text-muted uppercase"
								for="language"
							>
								Language
							</label>
							<input
								id="language"
								class="w-full rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
								bind:value={enrichment.language}
							/>
						</div>
						<div class="space-y-1">
							<label
								class="block text-xs font-semibold tracking-wide text-muted uppercase"
								for="enrichment-model"
							>
								Model (optional)
							</label>
							<input
								id="enrichment-model"
								class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
								value={enrichment.model ?? ''}
								oninput={(event) => {
									if (!enrichment) return;
									const next = event.currentTarget.value.trim();
									// Empty means "use the credential's default" — an empty string would be a model
									// name of nothing.
									enrichment.model = next === '' ? null : next;
								}}
								placeholder="the credential's default"
							/>
						</div>
						<div class="space-y-1">
							<label
								class="block text-xs font-semibold tracking-wide text-muted uppercase"
								for="alt-field"
							>
								Alt text goes in
							</label>
							<input
								id="alt-field"
								class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
								value={enrichment.alt_text_field ?? ''}
								oninput={(event) => {
									if (!enrichment) return;
									const next = event.currentTarget.value.trim();
									enrichment.alt_text_field = next === '' ? null : next;
								}}
								placeholder="leave empty to write none"
							/>
						</div>
						<div class="space-y-1">
							<label
								class="block text-xs font-semibold tracking-wide text-muted uppercase"
								for="description-field"
							>
								Description goes in
							</label>
							<input
								id="description-field"
								class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
								value={enrichment.description_field ?? ''}
								oninput={(event) => {
									if (!enrichment) return;
									const next = event.currentTarget.value.trim();
									enrichment.description_field = next === '' ? null : next;
								}}
								placeholder="leave empty to write none"
							/>
						</div>
					</div>

					<label class="flex items-center gap-2 text-sm">
						<input type="checkbox" bind:checked={enrichment.suggest_tags} />
						Suggest tags from the taxonomy
					</label>

					<div class="flex items-center gap-3">
						<button
							type="submit"
							class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
							disabled={busy === 'enrichment'}
						>
							{busy === 'enrichment' ? 'Saving…' : 'Save description settings'}
						</button>
						<a class="text-sm underline" href={resolve('/review')}>Review what it wrote</a>
					</div>
				</form>
			{/if}
		</section>

		<section class="space-y-3" aria-labelledby="credentials-heading">
			<h2 id="credentials-heading" class="text-lg font-medium">Credentials</h2>
			{#if credentials.length === 0}
				<p class="text-sm text-muted">None yet. Add one below.</p>
			{:else}
				<ul class="divide-y divide-line rounded-md border border-line">
					{#each credentials as credential (credential.id)}
						<li class="space-y-2 p-3">
							<div class="flex flex-wrap items-center gap-2">
								<span class="font-medium">{credential.label}</span>
								<span class="font-mono text-xs text-muted">{credential.hint}</span>
								<span class="rounded bg-surface px-1.5 py-0.5 text-xs">{credential.provider}</span>
								<span class="font-mono text-xs text-muted">{credential.default_model}</span>
								{#if credential.is_default}
									<span
										class="rounded bg-state-rights-allowed/18 px-1.5 py-0.5 text-xs text-state-rights-allowed-fg"
									>
										In use
									</span>
								{/if}
								{#if !credential.is_active}
									<span class="rounded bg-surface px-1.5 py-0.5 text-xs">Withdrawn</span>
								{/if}
								{#if credential.needs_resealing}
									<span
										class="rounded bg-surface px-1.5 py-0.5 text-xs"
										title="Sealed under a retired key"
									>
										Needs re-sealing
									</span>
								{/if}
							</div>

							<div class="flex flex-wrap items-center gap-3 text-sm">
								<button
									type="button"
									class="underline disabled:opacity-50"
									disabled={busy === credential.id}
									onclick={() => verify(credential.id)}
								>
									{busy === credential.id ? 'Checking…' : 'Check it works'}
								</button>
								{#if !credential.is_default && credential.is_active}
									<button
										type="button"
										class="underline disabled:opacity-50"
										disabled={busy === credential.id}
										onclick={() =>
											act(
												credential.id,
												() => makeAiCredentialDefault(credential.id),
												(row) => `${row.label} is now the credential enrichment uses.`
											)}
									>
										Use this one
									</button>
								{/if}
								<button
									type="button"
									class="underline disabled:opacity-50"
									disabled={busy === credential.id}
									onclick={() =>
										act(
											credential.id,
											() => setAiCredentialActive(credential.id, !credential.is_active),
											(row) =>
												row.is_active ? `${row.label} restored.` : `${row.label} withdrawn.`
										)}
								>
									{credential.is_active ? 'Withdraw' : 'Restore'}
								</button>
								<button
									type="button"
									class="underline"
									onclick={() => {
										rotating = rotating === credential.id ? null : credential.id;
										rotationKey = '';
									}}
								>
									Replace key
								</button>
							</div>

							{#if rotating === credential.id}
								<div class="flex flex-wrap items-end gap-2">
									<div class="space-y-1">
										<label
											class="block text-xs font-semibold tracking-wide text-muted uppercase"
											for="rotate-{credential.id}"
										>
											New key for {credential.label}
										</label>
										<input
											id="rotate-{credential.id}"
											type="password"
											autocomplete="off"
											class="w-72 rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
											bind:value={rotationKey}
										/>
									</div>
									<button
										type="button"
										class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
										disabled={rotationKey.trim() === '' || busy === credential.id}
										onclick={() => rotate(credential.id)}
									>
										Replace
									</button>
								</div>
							{/if}

							{#if verified[credential.id]}
								{@const result = verified[credential.id]}
								<p
									role="status"
									class="rounded-md p-2 text-sm {result.ok
										? 'bg-state-rights-allowed/18 text-state-rights-allowed-fg'
										: 'bg-state-rights-denied/18 text-state-rights-denied-fg'}"
								>
									{#if result.ok}
										Answered by {result.model}: “{result.detail}”
									{:else}
										{result.detail}{result.worth_retrying ? ' Worth another try.' : ''}
									{/if}
								</p>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="space-y-3" aria-labelledby="add-heading">
			<h2 id="add-heading" class="text-lg font-medium">Add a credential</h2>
			<form class="space-y-4" onsubmit={add}>
				<div class="grid gap-4 sm:grid-cols-2">
					<div class="space-y-1">
						<label
							class="block text-xs font-semibold tracking-wide text-muted uppercase"
							for="provider"
						>
							Wire format
						</label>
						<select
							id="provider"
							class="w-full rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
							bind:value={provider}
						>
							<option value="anthropic">Anthropic</option>
							<option value="openai_compatible">OpenAI-compatible</option>
						</select>
					</div>
					<div class="space-y-1">
						<label
							class="block text-xs font-semibold tracking-wide text-muted uppercase"
							for="label"
						>
							Name
						</label>
						<input
							id="label"
							class="w-full rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
							bind:value={label}
							placeholder="Marketing team key"
							required
						/>
					</div>
					<div class="space-y-1">
						<label
							class="block text-xs font-semibold tracking-wide text-muted uppercase"
							for="model"
						>
							Model
						</label>
						<input
							id="model"
							class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
							bind:value={defaultModel}
							required
						/>
					</div>
					<div class="space-y-1">
						<label
							class="block text-xs font-semibold tracking-wide text-muted uppercase"
							for="endpoint"
						>
							Endpoint {endpointNeeded ? '' : '(optional)'}
						</label>
						<input
							id="endpoint"
							class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
							bind:value={baseUrl}
							required={endpointNeeded}
							placeholder={endpointNeeded
								? 'https://api.moonshot.ai/v1'
								: 'a gateway, if you use one'}
							aria-describedby="endpoint-note"
						/>
						<p id="endpoint-note" class="text-xs text-muted">
							{#if endpointNeeded}
								Include the version segment — it differs per vendor, so there is nothing to guess.
							{:else}
								Left empty, calls go to api.anthropic.com.
							{/if}
						</p>
					</div>
				</div>

				<div class="space-y-1">
					<label
						class="block text-xs font-semibold tracking-wide text-muted uppercase"
						for="api-key"
					>
						Provider key
					</label>
					<input
						id="api-key"
						type="password"
						autocomplete="off"
						class="w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-sm"
						bind:value={apiKey}
						required
						aria-describedby="api-key-note"
					/>
					<p id="api-key-note" class="text-xs text-muted">
						Encrypted before it is stored and never readable again — not by this screen, not by the
						API. Only the last four characters are kept in the clear, to tell two keys apart.
					</p>
				</div>

				<div class="flex items-center gap-3">
					<label class="flex items-center gap-2 text-sm">
						<input type="checkbox" bind:checked={makeDefault} />
						Use this one for enrichment
					</label>
					<button
						type="submit"
						class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg disabled:opacity-50"
						disabled={busy === 'add'}
					>
						{busy === 'add' ? 'Storing…' : 'Store key'}
					</button>
				</div>
			</form>
		</section>
	{/if}
</div>
