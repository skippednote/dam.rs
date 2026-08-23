<script lang="ts">
	/**
	 * Webhooks: where damrs sends events, and what happened to them.
	 *
	 * ## The secret is shown once, and the screen says so loudly
	 *
	 * A receiver cannot verify a delivery without the signing key, so it has to be shown — and returning it on
	 * every read would put it in the response of an endpoint an integration polls. So it appears once, in a
	 * panel that stays until dismissed, with the verification recipe beside it. Anything less and somebody
	 * closes the dialog and has to delete the subscription to get another.
	 *
	 * ## A disabled subscription is the loudest thing here
	 *
	 * The system disables an endpoint that abandoned five deliveries in a row, and until somebody notices,
	 * nothing is being delivered at all. That is the one state on this screen worth interrupting for, so it
	 * carries the reason the system recorded and a button to undo it.
	 *
	 * ## The log is the diagnosis, so it shows the receiver's own words
	 *
	 * `last_error` is what the endpoint said, or what the network did. A row saying "failed" without it is a
	 * support ticket; with it, an operator can usually fix the thing themselves.
	 *
	 * ## Retry appears only on a dead letter
	 *
	 * Reviving something still in flight would break the per-asset ordering the outbox exists to keep, so the
	 * server refuses it — and offering a button that 404s would be worse than offering none.
	 */
	import { onMount } from 'svelte';
	import {
		ApiError,
		createWebhook,
		deleteWebhook,
		enableWebhook,
		listWebhooks,
		retryWebhookDelivery,
		webhookDeliveries,
		type Webhook,
		type WebhookCreated,
		type WebhookDelivery
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	/** Every event damrs emits. Kept in step with `dam_db::webhooks::kind`. */
	const KINDS = [
		['asset.published', 'became visible on a public surface'],
		['asset.unpublished', 'was withdrawn from public surfaces'],
		['asset.version_created', 'got a new current version, keeping its id'],
		['asset.deleted', 'was deleted'],
		['asset.metadata_updated', 'had metadata changed'],
		['asset.status_changed', 'was archived or restored']
	] as const;

	let hooks = $state<Webhook[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');

	/** The one-time secret panel. Stays until dismissed. */
	let created = $state<WebhookCreated | null>(null);

	let newUrl = $state('');
	/** Which kinds are ticked. Empty means all of them, which is what the server stores. */
	let chosen = $state<string[]>([]);
	let creating = $state(false);

	let open = $state('');
	let log = $state<WebhookDelivery[]>([]);
	let busy = $state('');

	async function load() {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			hooks = await listWebhooks();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Webhook administration needs Manage.'
					: 'Could not read the webhooks.';
		} finally {
			loading = false;
		}
	}

	onMount(load);

	async function make(event: SubmitEvent) {
		event.preventDefault();
		creating = true;
		error = '';
		try {
			const made = await createWebhook(newUrl.trim(), chosen);
			created = made;
			hooks = await listWebhooks();
			// `CreatedView` flattens the subscription, so the fields are inline rather than nested — the
			// generated type says so, which is how this got caught before anybody saw `undefined` on screen.
			notice = `Registered ${made.url}.`;
			newUrl = '';
			chosen = [];
		} catch (caught) {
			// The 422 body explains exactly which rule the URL broke — https, credentials, or a private
			// address — and that sentence is more useful than anything this page could say instead.
			error = caught instanceof Error ? caught.message : 'Could not register that endpoint.';
		} finally {
			creating = false;
		}
	}

	async function drop(hook: Webhook) {
		busy = hook.id;
		error = '';
		try {
			await deleteWebhook(hook.id);
			hooks = hooks.filter((one) => one.id !== hook.id);
			if (open === hook.id) open = '';
			notice = `${hook.url} removed, with anything still queued for it.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not remove that endpoint.';
		} finally {
			busy = '';
		}
	}

	async function reenable(hook: Webhook) {
		busy = hook.id;
		error = '';
		try {
			const enabled = await enableWebhook(hook.id);
			hooks = hooks.map((one) => (one.id === enabled.id ? enabled : one));
			notice = `${enabled.url} enabled. Anything still queued will be tried again.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not enable that endpoint.';
		} finally {
			busy = '';
		}
	}

	async function show(hook: Webhook) {
		if (open === hook.id) {
			open = '';
			log = [];
			return;
		}
		open = hook.id;
		log = [];
		error = '';
		try {
			log = await webhookDeliveries(hook.id);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not read the deliveries.';
		}
	}

	async function retry(hook: Webhook, delivery: WebhookDelivery) {
		busy = delivery.id;
		error = '';
		try {
			await retryWebhookDelivery(hook.id, delivery.id);
			log = await webhookDeliveries(hook.id);
			notice = 'Queued for another round of attempts.';
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not retry that delivery.';
		} finally {
			busy = '';
		}
	}

	function toggle(kind: string) {
		chosen = chosen.includes(kind) ? chosen.filter((one) => one !== kind) : [...chosen, kind];
	}

	function when(at: string): string {
		return new Date(at).toLocaleString();
	}

	/** What a state means, in a word an operator can act on. */
	const STATES: Record<string, string> = {
		pending: 'waiting to be sent',
		delivering: 'being sent now',
		delivered: 'accepted',
		failed: 'failed, will retry',
		dead: 'abandoned'
	};
</script>

<svelte:head><title>Webhooks · damrs</title></svelte:head>

<div class="space-y-6 p-4">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Webhooks</h1>
		<p class="max-w-2xl text-sm text-muted">
			damrs posts a signed JSON event to each endpoint when an asset changes. Events carry ids,
			never bytes: a receiver reads the asset through the API with its own credential, so
			withdrawing rights takes effect downstream instead of being cached somewhere it cannot be
			undone.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>
	{#if notice}
		<p class="text-xs text-muted">{notice}</p>
	{/if}

	{#if created}
		<!--
			A panel rather than a toast, and it stays until dismissed. This is the only time the signing key is
			ever shown; a message that fades would mean deleting the subscription to get another.
		-->
		<div class="space-y-2 rounded-md border border-accent p-3">
			<h2 class="text-sm font-semibold tracking-tight">
				Save this signing key — it is not shown again
			</h2>
			<p class="font-mono text-xs break-all select-all">{created.secret}</p>
			<p class="max-w-3xl text-xs text-muted">{created.signature_note}</p>
			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
				onclick={() => (created = null)}
			>
				I have saved it
			</button>
		</div>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">Not connected.</p>
	{:else}
		<form class="space-y-3 rounded-md border border-line p-3" onsubmit={make}>
			<div class="flex flex-wrap items-end gap-3">
				<label class="space-y-1 text-xs">
					<span class="block text-muted">Endpoint URL</span>
					<input
						bind:value={newUrl}
						required
						type="url"
						placeholder="https://example.com/damrs/hook"
						class="w-96 rounded-md border border-line bg-surface px-2 py-1 font-mono text-sm"
					/>
				</label>
				<button
					type="submit"
					disabled={creating}
					class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised disabled:opacity-50"
				>
					{creating ? 'Registering…' : 'Register'}
				</button>
			</div>

			<fieldset class="space-y-1">
				<legend class="text-xs text-muted">
					Events — tick none to receive all of them, which is the usual choice
				</legend>
				<div class="flex flex-wrap gap-x-4 gap-y-1">
					{#each KINDS as [kind, description] (kind)}
						<label class="flex items-center gap-2 text-xs">
							<input
								type="checkbox"
								checked={chosen.includes(kind)}
								onchange={() => toggle(kind)}
							/>
							<span class="font-mono">{kind}</span>
							<span class="text-muted">— an asset {description}</span>
						</label>
					{/each}
				</div>
			</fieldset>

			<p class="text-xs text-muted">
				Must be https, must not carry credentials in the URL, and must not be a private or loopback
				address — damrs posts to it from the server, so an internal address would let a webhook
				reach things this tenant cannot.
			</p>
		</form>

		{#if loading}
			<p class="text-sm text-muted">Loading…</p>
		{:else if hooks.length === 0}
			<p class="text-sm text-muted">
				No endpoints registered, so nothing is being sent anywhere. Events are still recorded on
				each asset's history.
			</p>
		{/if}

		<ul class="space-y-3">
			{#each hooks as hook (hook.id)}
				<li
					class="space-y-2 rounded-md border p-3 {hook.active
						? 'border-line'
						: 'border-state-rights-denied'}"
				>
					<div class="flex flex-wrap items-baseline gap-3">
						<span class="font-mono text-sm break-all">{hook.url}</span>
						{#if hook.active}
							<span class="rounded border border-line px-1.5 py-0.5 text-xs">active</span>
						{:else}
							<span
								class="rounded border border-state-rights-denied px-1.5 py-0.5 text-xs text-state-rights-denied-fg"
							>
								disabled — nothing is being delivered
							</span>
						{/if}
						<span class="text-xs text-muted">
							{hook.event_kinds.length === 0
								? 'all events'
								: `${hook.event_kinds.length} event ${hook.event_kinds.length === 1 ? 'kind' : 'kinds'}`}
						</span>
						{#if hook.consecutive_failures > 0}
							<span class="text-xs text-muted tabular-nums">
								{hook.consecutive_failures} abandoned in a row
							</span>
						{/if}

						<button
							type="button"
							class="ml-auto rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
							aria-expanded={open === hook.id}
							onclick={() => show(hook)}
						>
							{open === hook.id ? 'Hide deliveries' : 'Deliveries'}
						</button>
						{#if !hook.active}
							<button
								type="button"
								class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
								disabled={busy === hook.id}
								onclick={() => reenable(hook)}
							>
								Enable
							</button>
						{/if}
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === hook.id}
							aria-label="Remove {hook.url}"
							onclick={() => drop(hook)}
						>
							Remove
						</button>
					</div>

					{#if hook.event_kinds.length > 0}
						<p class="font-mono text-xs text-muted">{hook.event_kinds.join(' · ')}</p>
					{/if}

					{#if hook.disabled_reason}
						<!-- The system's own sentence. It names the count and the last error, which is what an
						     operator needs before deciding whether enabling it again will help. -->
						<p class="text-xs text-state-rights-denied-fg">{hook.disabled_reason}</p>
					{/if}

					{#if open === hook.id}
						<div class="space-y-1 border-t border-line pt-2">
							{#if log.length === 0}
								<p class="text-xs text-muted">Nothing sent to this endpoint yet.</p>
							{:else}
								<ul class="space-y-1">
									{#each log as delivery (delivery.id)}
										<li class="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-xs">
											<span class="font-mono">{delivery.event_kind}</span>
											<span
												class={delivery.state === 'dead' || delivery.state === 'failed'
													? 'text-state-rights-denied-fg'
													: 'text-muted'}
											>
												{STATES[delivery.state] ?? delivery.state}
											</span>
											{#if delivery.attempts > 0}
												<span class="text-muted tabular-nums">
													{delivery.attempts}
													{delivery.attempts === 1 ? 'attempt' : 'attempts'}
												</span>
											{/if}
											{#if delivery.response_status !== null && delivery.response_status !== undefined}
												<span class="text-muted tabular-nums">HTTP {delivery.response_status}</span>
											{/if}
											<time datetime={delivery.created_at} class="text-muted">
												{when(delivery.created_at)}
											</time>
											{#if delivery.state === 'dead'}
												<button
													type="button"
													class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
													disabled={busy === delivery.id}
													onclick={() => retry(hook, delivery)}
												>
													Retry
												</button>
											{/if}
											{#if delivery.last_error}
												<!-- The receiver's own words, or the network's. A row saying "failed" without
												     this is a support ticket; with it, an operator can usually fix it. -->
												<p class="w-full text-muted">{delivery.last_error}</p>
											{/if}
										</li>
									{/each}
								</ul>
							{/if}
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</div>
