<!--
	Which form this asset uses, and the ability to change it.

	Shown above the metadata editor rather than buried in a menu, because it is the thing that explains the
	editor: a field somebody expects to see and cannot is almost always this, and a panel that shows a short
	form without saying which form it is gives them nothing to act on.

	Changing it re-forms the asset. Values already stored are untouched — they stay in the document and become
	visible again if the field returns — so this is a display decision rather than a destructive one, and the
	copy says so.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		ApiError,
		listMetadataTypes,
		readAssetType,
		setAssetType,
		type AssetTypeView,
		type MetadataTypeRow
	} from '$lib/api/client';

	let {
		assetId,
		onresolved
	}: {
		assetId: string;
		/**
		 * The field keys that apply to this asset, whenever they change.
		 *
		 * `null` means the tenant has defined no types, so every field applies — which is what the panel
		 * showed before types existed. The panel filters its form to these keys, and that is not cosmetic:
		 * a form offering a field the asset's type excludes is a form whose save the API refuses, with the
		 * error landing on an input the user was invited to fill in.
		 */
		onresolved: (fieldKeys: string[] | null) => void;
	} = $props();

	let view = $state<AssetTypeView | null>(null);
	let types = $state<MetadataTypeRow[]>([]);
	let error = $state('');
	let busy = $state(false);

	/**
	 * Reloads for the selected asset.
	 *
	 * `assetId` is the only tracked read: everything the body writes is also read by it, and tracking those
	 * would make each write re-run the effect. Same trap the bulk bar hit.
	 */
	$effect(() => {
		const id = assetId;
		untrack(() => {
			error = '';
			void (async () => {
				try {
					const [resolved, defined] = await Promise.all([readAssetType(id), listMetadataTypes()]);
					view = resolved;
					types = defined;
					onresolved(defined.length === 0 ? null : resolved.field_keys);
				} catch (caught) {
					error = caught instanceof ApiError ? caught.message : 'Could not read the asset type.';
				}
			})();
		});
	});

	async function choose(value: string) {
		busy = true;
		error = '';
		try {
			view = await setAssetType(assetId, value === '' ? null : value);
			onresolved(view.field_keys);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not change the asset type.';
		} finally {
			busy = false;
		}
	}
</script>

<!-- Nothing at all when the tenant has defined no types: there is no choice to make, and a control offering
     one option is worse than no control. -->
{#if types.length > 0 && view}
	<div class="space-y-1">
		<label class="flex items-center justify-between gap-2 text-sm">
			<span class="text-xs font-semibold tracking-wide text-muted uppercase">Form</span>
			<select
				class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
				value={view.metadata_type_id ?? ''}
				disabled={busy}
				onchange={(event) => choose(event.currentTarget.value)}
			>
				<!-- "Default" rather than "none": clearing the type does not remove the form, it falls back to
				     the tenant's default. Labelling it "none" would promise an empty form. -->
				<option value="">Default for this file type</option>
				{#each types as type (type.id)}
					<option value={type.id}>{type.label}</option>
				{/each}
			</select>
		</label>
		<p class="text-xs text-muted">
			{view.field_keys.length} field{view.field_keys.length === 1 ? '' : 's'} on this form. Changing it
			changes which fields you can edit; values already saved are kept either way.
		</p>
		{#if error}
			<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
		{/if}
	</div>
{/if}
