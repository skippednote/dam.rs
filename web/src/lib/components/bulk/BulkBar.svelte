<!--
	The bulk-operations bar: appears when the grid has a selection, and turns it into one operation.

	## The number in the confirmation is the server's number, not the selection's

	Confirming always goes through `/bulk/preview` first, because the server filters the selection through the
	caller's own scope — a stale grid legitimately holds ids that were re-scoped a moment ago. Confirming "delete
	42" and having 38 happen is the kind of mismatch that erodes trust in every number the UI shows, so the
	dialog says both: what will happen, and how many of the selection fell outside the caller's scope.

	## Progress is polled and the end state is the server's

	`partial` is rendered as exactly that — done and failed counts, with the failed rows' own reasons — because
	the whole point of the backend's `partial` state is that a UI cannot put a green tick over failures.

	## Destructive confirmation is inline, not a browser confirm()

	A `confirm()` blocks the event loop and (in this harness) the automation driving it; an inline confirm keeps
	focus management ours and lets the dialog carry the preview numbers.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		addToCollection,
		ApiError,
		bulkStatus,
		createBulk,
		listCollections,
		listPeople,
		openRound,
		placeOrder,
		previewBulk,
		type BulkPreview,
		type BulkStatus,
		type Collection,
		type FieldDefinition,
		type Person
	} from '$lib/api/client';

	let {
		assetIds,
		fields,
		onfinished,
		onclear
	}: {
		/** The selection, as the grid reports it. */
		assetIds: string[];
		/** The tenant's definitions, so the metadata flow offers real fields. */
		fields: FieldDefinition[];
		/** Called when an operation reaches a terminal state, so the page refreshes what changed. */
		onfinished: () => void;
		/** Called to clear the grid's selection. */
		onclear: () => void;
	} = $props();

	type Flow =
		| { step: 'idle' }
		| { step: 'confirm'; kind: string; preview: BulkPreview; params: Record<string, unknown> }
		| { step: 'running'; status: BulkStatus }
		| { step: 'done'; status: BulkStatus };

	let flow = $state<Flow>({ step: 'idle' });
	let error = $state('');
	/** The metadata mini-form. */
	let metadataOpen = $state(false);
	/**
	 * The collection picker.
	 *
	 * Not routed through the bulk machinery, unlike publish and archive, and the reason is what a bulk
	 * operation is *for*: an actor, a target count and a per-item outcome recorded as an audit trail. Adding to
	 * a collection is arranging a working set — reversible, unaudited by design, and expected to be done
	 * dozens of times an hour. Putting it through `bulk_operations` would fill that table with noise and make
	 * the rows that matter — deletions, publications — harder to find.
	 */
	let collectionsOpen = $state(false);
	let collections = $state<Collection[]>([]);
	let addingTo = $state('');
	let addedNote = $state('');
	/**
	 * The order mini-form (Q.13).
	 *
	 * Here rather than in the detail panel because an order is nearly always for several assets — one photograph
	 * is a download, a shoot is a request — and the selection is where several assets already exist.
	 */
	let orderOpen = $state(false);
	let orderPurpose = $state('');
	let ordered = $state('');
	/**
	 * The review mini-form (M6b).
	 *
	 * Here, and only here, because the selection *is* the round: a round is a fixed set of assets, and asking
	 * for one from anywhere else would mean building a second way to choose assets. The set is snapshotted when
	 * the round opens — a reviewer who approved eleven pictures did not approve a twelfth added afterwards — so
	 * there is deliberately no way to widen a round later.
	 */
	let reviewOpen = $state(false);
	let reviewTitle = $state('');
	let people = $state<Person[]>([]);
	let reviewers = $state<string[]>([]);
	let reviewed = $state('');
	let metadataField = $state('');
	let metadataValue = $state('');

	const editable = $derived(fields.filter((field) => !field.read_only));

	/**
	 * A change of selection abandons an unconfirmed flow — the numbers it previewed are for another set.
	 *
	 * `flow` is read under `untrack`, and that is load-bearing rather than tidy: an effect's dependencies are
	 * whatever it reads, so reading `flow.step` tracked would make *setting* `flow` to `confirm` re-run this
	 * effect — which would see `confirm` and reset it to `idle` in the same tick. The dialog flashed open and
	 * closed itself, and the e2e case caught it: the bar snapped back to "2 selected" before the assertion.
	 * Only `assetIds` may trigger this.
	 */
	$effect(() => {
		void assetIds;
		untrack(() => {
			if (flow.step === 'confirm') flow = { step: 'idle' };
		});
	});

	/**
	 * Sends the selection as an order.
	 *
	 * Not a bulk operation: an order is a request for somebody's decision, not work to execute, so it does not go
	 * through the preview/confirm flow. What it *does* share is the honesty about numbers — the server narrows the
	 * selection to what the requester may see, and the reply says how many that was.
	 */
	function submitOrder() {
		error = '';
		ordered = '';
		void (async () => {
			try {
				const order = await placeOrder({ asset_ids: assetIds, purpose: orderPurpose.trim() });
				const count = order.items.length;
				ordered =
					count === assetIds.length
						? `${order.reference} sent for approval.`
						: `${order.reference} sent for approval — ${count} of ${assetIds.length} are yours to ask for.`;
				orderOpen = false;
				orderPurpose = '';
			} catch (caught) {
				error = caught instanceof ApiError ? caught.message : 'That order could not be sent.';
			}
		})();
	}

	/**
	 * Opens the review form, loading the people who could be asked.
	 *
	 * The list is loaded on opening rather than on mount: this bar renders on every selection change, and a
	 * tenant's directory is not worth a request per click of a checkbox.
	 */
	async function openReview() {
		reviewOpen = !reviewOpen;
		reviewed = '';
		if (!reviewOpen) return;
		try {
			people = await listPeople();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the people list.';
			reviewOpen = false;
		}
	}

	function toggleReviewer(id: string) {
		reviewers = reviewers.includes(id) ? reviewers.filter((one) => one !== id) : [...reviewers, id];
	}

	function submitReview() {
		error = '';
		reviewed = '';
		void (async () => {
			try {
				const round = await openRound({
					title: reviewTitle.trim(),
					asset_ids: assetIds,
					reviewer_ids: reviewers
				});
				// The server's count, not the selection's — it narrows the set to what the requester may see,
				// and claiming a picture went into a round when it did not is the mismatch that makes every
				// other number on this bar untrustworthy.
				reviewed =
					round.asset_count === assetIds.length
						? `Sent to ${round.reviewers.length} for review.`
						: `Sent to ${round.reviewers.length} for review — ${round.asset_count} of ${assetIds.length} are yours to send.`;
				reviewOpen = false;
				reviewTitle = '';
				reviewers = [];
			} catch (caught) {
				error =
					caught instanceof ApiError && caught.status === 403
						? 'Asking people to review is administration; this key does not hold Manage.'
						: caught instanceof Error
							? caught.message
							: 'That round could not be opened.';
			}
		})();
	}

	async function preview(kind: string, params: Record<string, unknown>) {
		error = '';
		try {
			const previewed = await previewBulk(kind, assetIds, params);
			if (previewed.target_count === 0) {
				error = 'Nothing in this selection is yours to manage.';
				return;
			}
			flow = { step: 'confirm', kind, preview: previewed, params };
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not preview the operation.';
		}
	}

	async function confirm() {
		if (flow.step !== 'confirm') return;
		const { kind, params } = flow;
		error = '';
		try {
			let status = await createBulk(kind, assetIds, params);
			flow = { step: 'running', status };
			// Polled rather than assumed: the worker is a separate process, and its progress is the truth.
			while (!status.terminal) {
				await new Promise((resolve) => setTimeout(resolve, 500));
				status = await bulkStatus(status.id);
				flow = { step: 'running', status };
			}
			flow = { step: 'done', status };
			metadataOpen = false;
			metadataField = '';
			metadataValue = '';
			onfinished();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'The operation could not be started.';
			flow = { step: 'idle' };
		}
	}

	function dismiss() {
		flow = { step: 'idle' };
		error = '';
	}

	async function openCollections() {
		collectionsOpen = !collectionsOpen;
		addedNote = '';
		if (!collectionsOpen) return;
		try {
			collections = await listCollections();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Collections are administration; this key does not hold Manage.'
					: 'Could not read the collections.';
			collectionsOpen = false;
		}
	}

	async function addTo(collection: Collection) {
		addingTo = collection.id;
		error = '';
		try {
			const added = await addToCollection(collection.id, assetIds);
			// The server's numbers, not the selection's. A stale grid can hold ids that were re-scoped a moment
			// ago, and reporting the selection size would claim work that did not happen — the same reason the
			// bulk confirmation shows the preview count rather than the selection count.
			addedNote =
				added.out_of_scope > 0
					? `Added ${added.added} to ${collection.label}. ${added.out_of_scope} were outside your scope.`
					: `Added ${added.added} to ${collection.label}.`;
			collections = await listCollections();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not add to that collection.';
		} finally {
			addingTo = '';
		}
	}

	function finishAndClear() {
		flow = { step: 'idle' };
		onclear();
	}

	function describe(kind: string, count: number): string {
		const assets = `${count} asset${count === 1 ? '' : 's'}`;
		if (kind === 'delete') return `Delete ${assets}`;
		// Named for what it does to the world rather than for the field it writes: publishing is what puts an
		// asset on a page anybody can reach, and a confirmation reading "Update 40 assets" hides that.
		if (kind === 'publish') return `Publish ${assets}`;
		if (kind === 'unpublish') return `Unpublish ${assets}`;
		// Named for the two different things they do, because the words are close enough to confuse and the
		// consequences are not: archiving takes assets out of circulation and leaves them instantly
		// fetchable, while restoring asks a storage provider for a copy and is billed per gigabyte.
		if (kind === 'archive') return `Archive ${assets}`;
		if (kind === 'unarchive') return `Return ${assets} to the active library`;
		if (kind === 'restore') return `Restore ${assets} from cold storage`;
		return `Update ${assets}`;
	}
</script>

{#if assetIds.length > 0 || flow.step === 'running' || flow.step === 'done'}
	<!--
		A toolbar, named, fixed above the grid's bottom edge. `role="toolbar"` because that is what it is: a
		set of controls operating on the current selection.
	-->
	<div
		role="toolbar"
		aria-label="Bulk operations"
		class="flex flex-wrap items-center gap-3 border-t border-line bg-surface px-4 py-2 text-sm shadow-lg"
	>
		{#if flow.step === 'idle'}
			<span class="tabular font-medium">
				{assetIds.length} selected
			</span>

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => (metadataOpen = !metadataOpen)}
				aria-expanded={metadataOpen}
			>
				Set metadata…
			</button>

			{#if metadataOpen}
				<!-- The mini-form: one field, one value. `null`-clearing and multi-field patches stay in the
				     detail panel; the bar is for the one edit somebody makes across a whole shoot. -->
				<label class="flex items-center gap-1.5">
					<span class="text-xs text-muted">Field</span>
					<select
						class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={metadataField}
					>
						<option value="" disabled>Choose…</option>
						{#each editable as field (field.key)}
							<option value={field.key}>{field.label}</option>
						{/each}
					</select>
				</label>
				<label class="flex items-center gap-1.5">
					<span class="text-xs text-muted">Value</span>
					<input
						class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={metadataValue}
					/>
				</label>
				<button
					type="button"
					class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg disabled:opacity-50"
					disabled={!metadataField}
					onclick={() => preview('metadata_set', { values: { [metadataField]: metadataValue } })}
				>
					Preview
				</button>
			{/if}

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => (orderOpen = !orderOpen)}
				aria-expanded={orderOpen}
			>
				Order…
			</button>

			{#if orderOpen}
				<!-- The reason, and nothing else. An order's other answers — format, intended use, recipients —
				     have sensible absences, but the reason is the entire question an approver answers, so it is
				     the one thing the bar asks for. -->
				<label class="flex items-center gap-1.5">
					<span class="text-xs text-muted">Why</span>
					<input
						class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={orderPurpose}
						placeholder="The spring brochure"
					/>
				</label>
				<button
					type="button"
					class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg disabled:opacity-50"
					disabled={!orderPurpose.trim()}
					onclick={() => submitOrder()}
				>
					Send request
				</button>
			{/if}

			{#if ordered}
				<span class="text-xs text-muted">{ordered}</span>
			{/if}

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => openReview()}
				aria-expanded={reviewOpen}
				data-testid="review-open"
			>
				Review…
			</button>

			{#if reviewOpen}
				<!--
					A title and who to ask. Nothing else, and the two are not optional: a round with no title is
					a line in somebody's list saying nothing, and a round with no reviewers asks nobody
					anything. The brief and the due date are on the round afterwards; these two are the
					question.
				-->
				<label class="flex items-center gap-1.5">
					<span class="text-xs text-muted">Title</span>
					<input
						class="rounded-md border border-line bg-bg px-2 py-1 text-sm"
						bind:value={reviewTitle}
						placeholder="Spring campaign crops"
						data-testid="review-title"
					/>
				</label>
				<fieldset class="flex flex-wrap items-center gap-x-2 gap-y-1">
					<legend class="sr-only">Who to ask</legend>
					{#each people as person (person.id)}
						<label class="flex items-center gap-1 text-xs">
							<input
								type="checkbox"
								class="rounded border-line"
								checked={reviewers.includes(person.id)}
								onchange={() => toggleReviewer(person.id)}
								data-testid="reviewer-{person.id}"
							/>
							<!-- The email too, because two colleagues can share a display name and asking the
							     wrong person to review a client's photographs is not a small mistake. -->
							<span>{person.name}</span>
							<span class="text-muted">{person.email}</span>
						</label>
					{/each}
				</fieldset>
				<button
					type="button"
					class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg disabled:opacity-50"
					disabled={!reviewTitle.trim() || reviewers.length === 0}
					onclick={() => submitReview()}
					data-testid="review-send"
				>
					Send for review
				</button>
			{/if}

			{#if reviewed}
				<span class="text-xs text-muted" data-testid="reviewed">{reviewed}</span>
			{/if}

			<!--
				Publication (Q.14). A separate control from metadata because it is not metadata: it is the act
				that admits an asset to a public page, which is why it goes through the same confirmation as a
				delete and why the confirmation names it.
			-->
			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => preview('publish', {})}
			>
				Publish…
			</button>

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => preview('unpublish', {})}
			>
				Unpublish…
			</button>

			<!--
				Archiving is the curation status: out of circulation, off the default grid, still instantly
				fetchable. Deliberately beside Unpublish rather than beside Delete, because that is the
				company it keeps — a reversible decision about visibility, not about existence.
			-->
			<!--
				Collections. Loaded on open rather than on mount: the bar appears on every selection and most
				selections never touch a collection, so a list fetched eagerly would be a request per
				click-to-select.
			-->
			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={openCollections}
			>
				Add to collection…
			</button>

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => preview('archive', {})}
			>
				Archive…
			</button>

			<!--
				And the storage half, which is a different act with a bill attached. Named "Restore" rather
				than "Unarchive" precisely so the two do not read as a pair: this one asks a provider for
				temporary copies and is charged per gigabyte, and the confirmation says so.
			-->
			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 hover:bg-raised"
				onclick={() => preview('restore', { tier: 'standard' })}
			>
				Restore…
			</button>

			<button
				type="button"
				class="rounded-md border border-line px-2.5 py-1 text-state-rights-denied-fg hover:bg-raised"
				onclick={() => preview('delete', {})}
			>
				Delete…
			</button>

			<button type="button" class="ml-auto text-xs text-muted underline" onclick={onclear}>
				Clear selection
			</button>
		{/if}

		{#if collectionsOpen && flow.step === 'idle'}
			<div class="flex w-full flex-wrap items-center gap-2 border-t border-line pt-2">
				{#if collections.length === 0}
					<p class="text-xs text-muted">
						No collections yet. Make one on the Collections screen — a portal publishes a
						collection, so that is where a public page starts.
					</p>
				{:else}
					{#each collections as collection (collection.id)}
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={addingTo === collection.id}
							onclick={() => addTo(collection)}
						>
							{collection.label}
							<span class="text-muted">{collection.item_count}</span>
						</button>
					{/each}
				{/if}
				{#if addedNote}
					<p role="status" class="text-xs text-muted">{addedNote}</p>
				{/if}
			</div>
		{:else if flow.step === 'confirm'}
			<span>
				{describe(flow.kind, flow.preview.target_count)}?
				{#if flow.preview.out_of_scope > 0}
					<span class="text-muted">
						({flow.preview.out_of_scope} of the selection {flow.preview.out_of_scope === 1
							? 'is'
							: 'are'} outside your scope and will be left alone.)
					</span>
				{/if}
			</span>
			<button
				type="button"
				class="rounded-md bg-accent px-2.5 py-1 font-medium text-accent-fg"
				onclick={confirm}
			>
				{describe(flow.kind, flow.preview.target_count)}
			</button>
			<button type="button" class="text-xs underline" onclick={dismiss}>Cancel</button>
		{:else if flow.step === 'running'}
			<!-- Announced as it changes: the person who started a 40,000-asset operation is listening for it. -->
			<span role="status" class="tabular">
				{flow.status.done_count + flow.status.failed_count} of {flow.status.target_count} —
				{flow.status.state}
			</span>
			<progress
				class="h-1 flex-1"
				max={flow.status.target_count}
				value={flow.status.done_count + flow.status.failed_count}
				aria-label="Bulk operation progress"
			></progress>
		{:else if flow.step === 'done'}
			<span role="status">
				{#if flow.status.state === 'completed'}
					Done — {flow.status.done_count} of {flow.status.target_count} applied.
				{:else}
					<!-- `partial` and `failed` name the failures; a green "done" over failures is the exact
					     thing the backend's state vocabulary exists to prevent. -->
					<span class="font-medium text-state-rights-denied-fg">
						{flow.status.state}: {flow.status.done_count} applied, {flow.status.failed_count} failed.
					</span>
					{#each flow.status.failures.slice(0, 3) as failure (failure.asset_id)}
						<span class="ml-2 text-xs text-muted">{failure.reason ?? 'failed'}</span>
					{/each}
				{/if}
			</span>
			<button type="button" class="ml-auto text-xs underline" onclick={finishAndClear}>
				Dismiss
			</button>
		{/if}

		{#if error}
			<span role="alert" class="text-xs text-state-rights-denied-fg">{error}</span>
		{/if}
	</div>
{/if}
