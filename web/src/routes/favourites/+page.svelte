<script lang="ts">
	/**
	 * The caller's favourites.
	 *
	 * A route rather than a filter on the browse page, because it is a *place*: somewhere to go and come back to,
	 * with its own URL to bookmark. `/assets?q=is:favourite` answers the same question in upload order, which is
	 * not the order a person built their own list in.
	 */
	import PrivateList from '$lib/components/grid/PrivateList.svelte';
</script>

<PrivateList
	kind="favourites"
	title="Favourites"
	explanation="Assets you have starred, most recently added first. Nobody else sees your favourites; the count on an asset says how many people starred it, never who."
	empty="Nothing starred yet. Press f on a grid cell, or use the star in an asset's panel."
/>
