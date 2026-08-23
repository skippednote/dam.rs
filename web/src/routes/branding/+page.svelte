<script lang="ts">
	/**
	 * Site branding: what this library calls itself, and what a portal inherits.
	 *
	 * ## The name field is empty when it is a fallback
	 *
	 * An unset `site_name` resolves to the tenant's display name, and the API says which it was. Pre-filling
	 * the field with the fallback would make the default look chosen — and then clearing it would be
	 * indistinguishable from never having set it. So the field is empty with the fallback as its placeholder,
	 * which is exactly what it is.
	 *
	 * ## The accent has a preview, because the number means nothing
	 *
	 * `#ff6600` is not a colour anybody can see. A swatch beside the field is the whole point of a colour
	 * picker, and the sentence under it says where the colour actually appears — which is mostly on portals,
	 * not here.
	 *
	 * ## The logo is an asset id, and this screen says so rather than offering an upload
	 *
	 * A logo is an asset: already governed, already backed up, already subject to the same rights rules. A
	 * second upload path for it would be a second place for an unlicensed image to appear. So the field takes
	 * an id, and the copy explains where to get one.
	 *
	 * ## Saving updates the header without a reload
	 *
	 * The shell reads the same store, so applying the response there is what makes the change visible where it
	 * matters — otherwise somebody saves a name and has to refresh to believe it.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import { ApiError, loadBranding, saveBranding, type Branding } from '$lib/api/client';
	import { branding as shell, DEFAULT_ACCENT } from '$lib/api/branding.svelte';
	import { session } from '$lib/api/session.svelte';

	let loaded = $state<Branding | null>(null);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');
	let saving = $state(false);

	/** The draft. `siteName` is empty when the loaded name is a fallback — see the component docs. */
	let siteName = $state('');
	let accent = $state(DEFAULT_ACCENT);
	let logoId = $state('');
	let supportEmail = $state('');

	onMount(async () => {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			const current = await loadBranding();
			loaded = current;
			siteName = current.site_name_is_default ? '' : current.site_name;
			accent = current.accent;
			logoId = current.logo_asset_id ?? '';
			supportEmail = current.support_email ?? '';
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Reading the branding needs a key with read access.'
					: 'Could not read the branding.';
		} finally {
			loading = false;
		}
	});

	async function save(event: SubmitEvent) {
		event.preventDefault();
		saving = true;
		error = '';
		notice = '';
		try {
			const saved = await saveBranding({
				site_name: siteName.trim(),
				// An empty field is an absence, not an empty id — the server would refuse "" as a uuid.
				logo_asset_id: logoId.trim() || null,
				accent: accent.trim().toLowerCase(),
				support_email: supportEmail.trim() || null
			});
			loaded = saved;
			// Read back: the server lowercases the accent and trims the strings, so the form shows what is
			// actually stored rather than what was typed.
			siteName = saved.site_name_is_default ? '' : saved.site_name;
			accent = saved.accent;
			logoId = saved.logo_asset_id ?? '';
			supportEmail = saved.support_email ?? '';
			// The header reads the same store, so this is what makes the change visible where it matters.
			shell.apply(saved);
			notice = 'Saved.';
		} catch (caught) {
			// The 422 says which rule failed — the colour format, or a logo outside this caller's scope — and
			// that sentence is more useful than anything this page could say instead.
			error = caught instanceof Error ? caught.message : 'Could not save the branding.';
		} finally {
			saving = false;
		}
	}
</script>

<svelte:head><title>Branding · damrs</title></svelte:head>

<div class="mx-auto max-w-2xl space-y-6 p-8">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Branding</h1>
		<p class="text-sm text-muted">
			What this library calls itself, and the colour a new portal inherits — so you set it once
			rather than once per press kit.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>
	{#if notice}
		<p class="text-xs text-muted">{notice}</p>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else}
		<form class="space-y-5" onsubmit={save}>
			<label class="block space-y-1">
				<span class="block text-xs text-muted">Library name</span>
				<input
					bind:value={siteName}
					maxlength="64"
					placeholder={loaded?.site_name ?? ''}
					class="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-sm"
				/>
				<span class="block text-xs text-muted">
					{#if loaded?.site_name_is_default}
						Empty, so your organisation's name is used: <strong>{loaded.site_name}</strong>.
					{:else}
						Clear it to fall back to your organisation's name.
					{/if}
				</span>
			</label>

			<div class="space-y-1">
				<label class="block space-y-1">
					<span class="block text-xs text-muted">Accent colour</span>
					<span class="flex items-center gap-2">
						<!--
							Two inputs on one value: the picker is how anybody actually chooses a colour, and the text
							field is how somebody pastes the hex from a brand guideline. Both write `accent`.
						-->
						<input
							type="color"
							bind:value={accent}
							aria-label="Accent colour picker"
							class="h-8 w-12 rounded border border-line bg-surface"
						/>
						<input
							bind:value={accent}
							pattern="#[0-9a-fA-F]{'{'}6{'}'}"
							class="w-32 rounded-md border border-line bg-surface px-2 py-1.5 font-mono text-sm"
						/>
					</span>
				</label>
				<p class="text-xs text-muted">
					Lowercase <span class="font-mono">#rrggbb</span>. It appears on your portals — the pages
					you share with people who have no account — and as the mark beside the name above. The
					application's own colours are left alone, because their light and dark text pairings are
					tuned for contrast.
				</p>
			</div>

			<label class="block space-y-1">
				<span class="block text-xs text-muted">Logo asset id</span>
				<input
					bind:value={logoId}
					placeholder="00000000-0000-0000-0000-000000000000"
					class="w-full rounded-md border border-line bg-surface px-2 py-1.5 font-mono text-sm"
				/>
				<span class="block text-xs text-muted">
					A logo is an asset: upload it to the library, open it, and copy its id. That way it is
					governed, backed up and rights-checked like everything else — rather than a second upload
					path where an unlicensed image could sit unnoticed.
				</span>
			</label>

			<label class="block space-y-1">
				<span class="block text-xs text-muted">Support address</span>
				<input
					bind:value={supportEmail}
					type="email"
					placeholder="help@example.com"
					class="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-sm"
				/>
				<span class="block text-xs text-muted">
					Shown in a portal's footer, where a recipient has no account and nobody to ask. Optional.
				</span>
			</label>

			<button
				type="submit"
				disabled={saving}
				class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised disabled:opacity-50"
			>
				{saving ? 'Saving…' : 'Save'}
			</button>
		</form>
	{/if}
</div>
