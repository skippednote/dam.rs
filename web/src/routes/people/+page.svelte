<script lang="ts">
	/**
	 * Who can use this library, and what they may do (G10·2a).
	 *
	 * ## Until this screen there was no way to add a colleague
	 *
	 * `tenant_members` has been read since the first migration and written in exactly one place — registering
	 * a connected site, which inserts a service account. Nobody could invite a person, change what they may
	 * see, or remove somebody who had left.
	 *
	 * ## The key appears once, and the panel says so
	 *
	 * There is no login flow: the application authenticates with an API key, so the key *is* the invitation.
	 * It is stored as a hash and cannot be read back, so a UI that showed it in a toast would lock somebody
	 * out before they arrived. Same panel, same sentence, as registering a site.
	 *
	 * ## Roles are offered, not typed
	 *
	 * `role_names` has no foreign key and the server ignores a name it cannot resolve, so `editors` for a role
	 * called `editor` produces somebody who can see nothing with nothing saying why. Checkboxes from
	 * `GET /roles` make that unreachable rather than validated.
	 *
	 * ## "Removed" has to be visible, so the key count is a column
	 *
	 * Somebody with live credentials and no roles still authenticates. The count is the difference between an
	 * account that has been removed and one that has been marked removed, and it is the number an
	 * administrator is actually being asked about.
	 *
	 * ## The rows the identity provider owns say so instead of failing
	 *
	 * A SCIM-managed account's roles come from the IdP and would be overwritten on the next sync, so the
	 * controls are absent and the row explains where to make the change. Removing them from *this tenant* is
	 * still offered: that is not editing the account, and refusing it would leave somebody who has left in
	 * place until the IdP was fixed.
	 */
	import { onMount } from 'svelte';
	import {
		ApiError,
		addMember,
		listMembers,
		listRoles,
		listScimClients,
		registerScimClient,
		removeMember,
		revokeScimClient,
		updateMember,
		type Member,
		type MemberAdded,
		type ScimClient,
		type ScimRegistered
	} from '$lib/api/client';

	let members = $state<Member[]>([]);
	let roles = $state<string[]>([]);
	let loading = $state(true);
	let error = $state('');
	let forbidden = $state(false);
	let notice = $state('');
	let busy = $state('');

	/** The one-time credential panel. Null until an invitation mints one. */
	let issued = $state<{ added: MemberAdded; email: string } | null>(null);

	let formOpen = $state(false);
	let email = $state('');
	let displayName = $state('');
	let wanted = $state<string[]>([]);
	let asAdmin = $state(false);

	/** Which row is being edited, and the roles it would be given. */
	let editing = $state<string | null>(null);
	let editRoles = $state<string[]>([]);
	let editAdmin = $state(false);

	/** Which row has been asked to be removed, so removal takes two deliberate actions. */
	let confirming = $state<string | null>(null);

	/** The identity providers wired up here, and the one-time token panel for a new one. */
	let providers = $state<ScimClient[]>([]);
	let providerToken = $state<{ registered: ScimRegistered } | null>(null);
	let providerForm = $state(false);
	let providerLabel = $state('');
	let revoking = $state<string | null>(null);

	const admins = $derived(members.filter((member) => member.is_tenant_admin).length);

	async function load() {
		loading = true;
		error = '';
		try {
			[members, roles, providers] = await Promise.all([
				listMembers(),
				listRoles(),
				listScimClients()
			]);
		} catch (cause) {
			if (cause instanceof ApiError && cause.status === 403) forbidden = true;
			else error = cause instanceof Error ? cause.message : 'Could not read the member list.';
		} finally {
			loading = false;
		}
	}

	onMount(load);

	function toggle(list: string[], role: string): string[] {
		return list.includes(role) ? list.filter((r) => r !== role) : [...list, role];
	}

	async function invite(event: SubmitEvent) {
		event.preventDefault();
		busy = 'invite';
		error = '';
		notice = '';
		try {
			const added = await addMember({
				email,
				display_name: displayName.trim() === '' ? null : displayName,
				role_names: wanted,
				is_tenant_admin: asAdmin
			});
			issued = { added, email: email.trim() };
			formOpen = false;
			email = '';
			displayName = '';
			wanted = [];
			asAdmin = false;
			await load();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not add them.';
		} finally {
			busy = '';
		}
	}

	function startEdit(member: Member) {
		editing = member.identity_id;
		editRoles = [...member.role_names];
		editAdmin = member.is_tenant_admin;
		confirming = null;
	}

	async function saveEdit(member: Member) {
		busy = member.identity_id;
		error = '';
		notice = '';
		try {
			await updateMember(member.identity_id, {
				role_names: editRoles,
				is_tenant_admin: editAdmin
			});
			editing = null;
			notice = `Updated ${member.email}.`;
			await load();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not change their roles.';
		} finally {
			busy = '';
		}
	}

	async function addProvider(event: SubmitEvent) {
		event.preventDefault();
		busy = 'provider';
		error = '';
		notice = '';
		try {
			providerToken = {
				registered: await registerScimClient({ label: providerLabel, scopes: ['Users'] })
			};
			providerForm = false;
			providerLabel = '';
			await load();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not add that provider.';
		} finally {
			busy = '';
		}
	}

	async function dropProvider(provider: ScimClient) {
		busy = provider.id;
		error = '';
		notice = '';
		try {
			await revokeScimClient(provider.id);
			revoking = null;
			// Deliberately not "and their accounts are gone": revoking stops the sync and leaves the people it
			// provisioned exactly where they are. Saying otherwise would be the more alarming and wrong claim.
			notice = `${provider.label} can no longer provision. The accounts it created are untouched.`;
			await load();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not revoke that provider.';
		} finally {
			busy = '';
		}
	}

	function contact(provider: ScimClient): string {
		if (!provider.last_sync_at) return 'Has never called';
		return `Last called ${new Date(provider.last_sync_at).toLocaleString()}${
			provider.last_sync_status ? ` · ${provider.last_sync_status}` : ''
		}`;
	}

	async function drop(member: Member) {
		busy = member.identity_id;
		error = '';
		notice = '';
		try {
			const result = await removeMember(member.identity_id);
			confirming = null;
			// The count, because it is what the action was actually for.
			notice =
				result.keys_revoked === 0
					? `${member.email} no longer has access. They had no keys to revoke.`
					: `${member.email} no longer has access. ${result.keys_revoked} ${
							result.keys_revoked === 1 ? 'key' : 'keys'
						} revoked${result.identity_disabled ? ', and their account is disabled' : ''}.`;
			await load();
		} catch (cause) {
			error = cause instanceof Error ? cause.message : 'Could not remove them.';
		} finally {
			busy = '';
		}
	}
</script>

<svelte:head>
	<title>People · damrs</title>
</svelte:head>

<div class="mx-auto max-w-4xl p-6">
	<div class="flex items-start justify-between gap-4">
		<div>
			<h1 class="text-xl font-semibold">People</h1>
			<p class="mt-1 text-sm text-muted">
				Who can use this library, and what each of them may see.
			</p>
		</div>
		{#if !forbidden}
			<button
				type="button"
				onclick={() => (formOpen = !formOpen)}
				class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg"
			>
				{formOpen ? 'Cancel' : 'Add someone'}
			</button>
		{/if}
	</div>

	{#if forbidden}
		<p class="mt-4 rounded-md bg-surface p-3 text-sm" role="status">
			Managing people needs administrator access. Ask whoever administers this library.
		</p>
	{:else}
		{#if issued}
			<!--
				Stays until dismissed. The key cannot be read back, so a panel that disappeared on its own would
				lock somebody out before they had it.
			-->
			<section
				class="mt-4 rounded-md border border-line bg-surface p-3"
				aria-labelledby="issued-heading"
				data-testid="issued"
			>
				<h2 id="issued-heading" class="text-sm font-semibold">
					{issued.email} can now sign in with this key
				</h2>
				<input
					readonly
					value={issued.added.api_key}
					class="mt-2 w-full rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs"
					aria-label="API key"
				/>
				<p class="mt-2 text-xs text-muted">{issued.added.warning}</p>
				<button
					type="button"
					onclick={() => (issued = null)}
					class="mt-2 rounded border border-line px-2.5 py-1 text-xs"
				>
					I have saved it
				</button>
			</section>
		{/if}

		{#if formOpen}
			<form onsubmit={invite} class="mt-4 rounded-md bg-surface p-3">
				<div class="flex flex-wrap gap-3">
					<label class="text-xs font-medium">
						Email
						<input
							type="email"
							bind:value={email}
							required
							class="mt-1 block w-64 rounded border border-line bg-bg px-2 py-1 text-sm"
						/>
					</label>
					<label class="text-xs font-medium">
						Name (optional)
						<input
							type="text"
							bind:value={displayName}
							class="mt-1 block w-48 rounded border border-line bg-bg px-2 py-1 text-sm"
						/>
					</label>
				</div>

				<fieldset class="mt-3">
					<legend class="text-xs font-medium">Roles</legend>
					{#if roles.length === 0}
						<p class="mt-1 text-xs text-muted">
							This library has no roles yet. Somebody added without one can sign in and see nothing.
						</p>
					{:else}
						<div class="mt-1 flex flex-wrap gap-3">
							{#each roles as role (role)}
								<label class="text-xs">
									<input
										type="checkbox"
										checked={wanted.includes(role)}
										onchange={() => (wanted = toggle(wanted, role))}
									/>
									{role}
								</label>
							{/each}
						</div>
					{/if}
				</fieldset>

				<label class="mt-3 block text-xs">
					<input type="checkbox" bind:checked={asAdmin} />
					Administrator — may add and remove everybody else, including you
				</label>

				<button
					type="submit"
					disabled={busy === 'invite'}
					class="mt-3 rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
				>
					{busy === 'invite' ? 'Adding…' : 'Add and issue a key'}
				</button>
			</form>
		{/if}

		{#if notice}
			<p class="mt-3 text-sm" role="status">{notice}</p>
		{/if}
		{#if error}
			<p class="mt-3 text-sm text-state-rights-denied-fg" role="alert">{error}</p>
		{/if}

		<!--
			Provisioning sits above the list because it explains it: a row marked "From your identity provider"
			only makes sense once you know a provider is wired up. The two are one screen for that reason —
			splitting them would put the cause and the effect in different places.
		-->
		<section class="mt-5 rounded-md bg-surface p-3" aria-labelledby="providers-heading">
			<div class="flex items-center justify-between gap-3">
				<h2 id="providers-heading" class="text-sm font-semibold">Identity providers</h2>
				<button
					type="button"
					onclick={() => (providerForm = !providerForm)}
					class="rounded border border-line px-2.5 py-1 text-xs"
				>
					{providerForm ? 'Cancel' : 'Connect a provider'}
				</button>
			</div>
			<p class="mt-1 text-xs text-muted">
				SCIM 2.0. A provider creates and removes accounts here as people join and leave — the
				removal is the half that matters, and it revokes their credentials rather than only marking
				them gone.
			</p>

			{#if providerToken}
				<div class="mt-3 rounded-md border border-line p-2" data-testid="provider-token">
					<p class="text-xs font-medium">
						{providerToken.registered.client.label} — paste this into the provider
					</p>
					<input
						readonly
						value={providerToken.registered.token}
						aria-label="Provisioning token"
						class="mt-1 w-full rounded border border-line bg-surface px-2 py-1 font-mono text-xs"
					/>
					<p class="mt-1 text-xs text-muted">{providerToken.registered.warning}</p>
					<button
						type="button"
						onclick={() => (providerToken = null)}
						class="mt-2 rounded border border-line px-2.5 py-1 text-xs"
					>
						I have saved it
					</button>
				</div>
			{/if}

			{#if providerForm}
				<form onsubmit={addProvider} class="mt-3 flex flex-wrap items-end gap-2">
					<label class="text-xs font-medium">
						Name it
						<input
							type="text"
							bind:value={providerLabel}
							required
							placeholder="Okta, Entra, …"
							class="mt-1 block w-56 rounded border border-line bg-bg px-2 py-1 text-sm"
						/>
					</label>
					<button
						type="submit"
						disabled={busy === 'provider' || providerLabel.trim() === ''}
						class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
					>
						{busy === 'provider' ? 'Connecting…' : 'Connect and issue a token'}
					</button>
					<p class="w-full text-xs text-muted">
						A name, because a stalled provider has to be identifiable. The token is shown once.
					</p>
				</form>
			{/if}

			{#if providers.length === 0}
				<p class="mt-2 text-xs text-muted">
					None connected. People are added by hand below, which is the whole story until a provider
					is wired up.
				</p>
			{:else}
				<ul class="mt-2 space-y-1.5">
					{#each providers as provider (provider.id)}
						<li class="text-xs" data-testid="provider-{provider.label}">
							<div class="flex flex-wrap items-baseline justify-between gap-2">
								<span class="font-medium">
									{provider.label}
									{#if provider.revoked_at}
										<span class="ml-1 rounded bg-raised px-1.5 py-0.5 font-medium">Revoked</span>
									{/if}
								</span>
								<span class="text-muted">{contact(provider)}</span>
							</div>
							{#if !provider.revoked_at}
								{#if revoking === provider.id}
									<p class="mt-1 text-muted">
										Revoke {provider.label}? It stops provisioning immediately and cannot be brought
										back. The accounts it created keep their access.
									</p>
									<div class="mt-1 flex gap-2">
										<button
											type="button"
											onclick={() => dropProvider(provider)}
											disabled={busy === provider.id}
											class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg disabled:opacity-50"
										>
											Revoke it
										</button>
										<button
											type="button"
											onclick={() => (revoking = null)}
											class="rounded border border-line px-2.5 py-1"
										>
											Keep it
										</button>
									</div>
								{:else}
									<button
										type="button"
										onclick={() => (revoking = provider.id)}
										class="mt-1 rounded border border-line px-2.5 py-1"
									>
										Revoke
									</button>
								{/if}
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		{#if loading && members.length === 0}
			<p class="mt-4 text-sm text-muted">Loading…</p>
		{:else}
			<ul class="mt-4 space-y-2">
				{#each members as member (member.identity_id)}
					<li class="rounded-md bg-surface p-3" data-testid="member-{member.email}">
						<div class="flex flex-wrap items-baseline justify-between gap-2">
							<div>
								<p class="text-sm font-medium">
									{member.display_name ?? member.email}
									{#if member.is_tenant_admin}
										<span class="ml-1 rounded bg-raised px-1.5 py-0.5 text-xs font-medium">
											Administrator
										</span>
									{/if}
									{#if member.scim_managed}
										<span class="ml-1 rounded bg-raised px-1.5 py-0.5 text-xs font-medium">
											From your identity provider
										</span>
									{/if}
								</p>
								<p class="text-xs text-muted">{member.email}</p>
							</div>
							<p class="text-xs text-muted">
								{#if member.is_tenant_admin}
									<!--
										An administrator gets every asset group without holding a role at all — see
										`auth`'s own case for it. So "no roles" must not read as "no access" here: it
										said "can sign in and see nothing" beside four administrators on the dev tenant,
										which is the opposite of true.
									-->
									Everything, as an administrator{#if member.role_names.length > 0}
										· {member.role_names.join(', ')}{/if}
								{:else if member.role_names.length === 0}
									No roles — can sign in and see nothing
								{:else}
									{member.role_names.join(', ')}
								{/if}
							</p>
						</div>

						<p class="mt-1 text-xs text-muted">
							{#if member.status !== 'active'}
								<!-- Since authentication allowlists `active`, this is not cosmetic: their keys do not work. -->
								Account {member.status} — their keys do not work ·
							{/if}
							{member.live_keys}
							{member.live_keys === 1 ? 'key' : 'keys'} in use
							{#if member.last_login_at}
								· last seen {new Date(member.last_login_at).toLocaleDateString()}
							{/if}
						</p>

						{#if editing === member.identity_id}
							<fieldset class="mt-2">
								<legend class="text-xs font-medium">Roles</legend>
								<div class="mt-1 flex flex-wrap gap-3">
									{#each roles as role (role)}
										<label class="text-xs">
											<input
												type="checkbox"
												checked={editRoles.includes(role)}
												onchange={() => (editRoles = toggle(editRoles, role))}
											/>
											{role}
										</label>
									{/each}
								</div>
							</fieldset>
							<label class="mt-2 block text-xs">
								<input type="checkbox" bind:checked={editAdmin} />
								Administrator
							</label>
							{#if member.is_tenant_admin && admins <= 1}
								<p class="mt-1 text-xs text-muted">
									This is the only administrator. Appoint another before removing this one, or
									nobody will be able to.
								</p>
							{/if}
							<div class="mt-2 flex gap-2">
								<button
									type="button"
									onclick={() => saveEdit(member)}
									disabled={busy === member.identity_id}
									class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
								>
									Save
								</button>
								<button
									type="button"
									onclick={() => (editing = null)}
									class="rounded border border-line px-2.5 py-1 text-xs"
								>
									Cancel
								</button>
							</div>
						{:else if confirming === member.identity_id}
							<p class="mt-2 text-xs">
								Remove {member.email}? Their {member.live_keys}
								{member.live_keys === 1 ? 'key' : 'keys'} will stop working immediately.
							</p>
							<div class="mt-2 flex gap-2">
								<button
									type="button"
									onclick={() => drop(member)}
									disabled={busy === member.identity_id}
									class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
								>
									Remove them
								</button>
								<button
									type="button"
									onclick={() => (confirming = null)}
									class="rounded border border-line px-2.5 py-1 text-xs"
								>
									Keep them
								</button>
							</div>
						{:else}
							<div class="mt-2 flex gap-2">
								{#if member.scim_managed}
									<p class="text-xs text-muted">
										Roles come from your identity provider. Change them there — an edit here would
										be overwritten on the next sync.
									</p>
								{:else}
									<button
										type="button"
										onclick={() => startEdit(member)}
										class="rounded border border-line px-2.5 py-1 text-xs"
									>
										Change roles
									</button>
								{/if}
								<button
									type="button"
									onclick={() => (confirming = member.identity_id)}
									class="rounded border border-line px-2.5 py-1 text-xs"
								>
									Remove
								</button>
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</div>
