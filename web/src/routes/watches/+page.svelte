<script lang="ts">
	/**
	 * The assets the caller is watching.
	 *
	 * Notifications do not exist yet (M6), and this page says so rather than implying a service that is not
	 * running — a list of things you are "watching" with nothing ever arriving is worse than an honest note.
	 */
	import PrivateList from '$lib/components/grid/PrivateList.svelte';
</script>

<PrivateList
	kind="watches"
	title="Watching"
	explanation="Assets you asked to be told about, most recently added first. Notifications are not switched on yet, so this records what you want to hear about rather than sending anything. Nobody is told how many people watch an asset."
	empty="Not watching anything. Use the Watch button in an asset's panel."
/>
