<script lang="ts">
	/**
	 * Tag vocabularies: the label set machine tagging scores against, and who may draw on it.
	 *
	 * ## The AI switch is the most consequential control on the page, and looks like it
	 *
	 * Ticking it puts every term in this vocabulary into the prompt of every enrichment call. So it is its own
	 * control with its own sentence, not a checkbox in a row of them — and it says what it costs, because a
	 * vocabulary of four thousand terms is not a prompt, it is a bill.
	 *
	 * ## Retire and merge, never delete
	 *
	 * There is no delete button because there is no delete endpoint. `asset_tags` cascades, so deleting a term
	 * untags every asset that carried it — years of somebody's work, gone quietly, noticed when a search comes
	 * back empty. Retiring keeps the assets and keeps the id resolving; merging moves the assets and leaves a
	 * pointer to where the meaning went.
	 *
	 * ## A retired term stays on screen
	 *
	 * Greyed, labelled, and showing what it merged into. Hiding it would make the list shorter and leave nobody
	 * able to answer "what happened to that tag" — which is the question a retirement creates.
	 *
	 * ## The slug is not editable, anywhere
	 *
	 * It is what a model answers with and what an import resolves. There is no field for it on the edit form,
	 * for the same reason a collection's key has none: the thing somebody wants to change is the label.
	 */
	import { onMount } from 'svelte';
	import {
		addVocabularyTerm,
		amendVocabularyTerm,
		ApiError,
		createVocabulary,
		listVocabularies,
		mergeVocabularyTerm,
		retireVocabularyTerm,
		setVocabularyAi,
		vocabularyTerms,
		type Vocabulary,
		type VocabularyTerm
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let vocabularies = $state<Vocabulary[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');

	/** Which vocabulary's terms are open. One at a time. */
	let open = $state('');
	let terms = $state<VocabularyTerm[]>([]);
	let busy = $state('');

	let newKey = $state('');
	let newLabel = $state('');

	let termSlug = $state('');
	let termLabel = $state('');
	let termSynonyms = $state('');

	/** The term being edited, and its draft. */
	let editing = $state('');
	let draft = $state({ label: '', synonyms: '', threshold: 0.35 });
	/** The term being merged, and where into. */
	let merging = $state('');
	let mergeInto = $state('');

	async function load() {
		if (!session.connected) {
			loading = false;
			return;
		}
		try {
			vocabularies = await listVocabularies();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Vocabulary administration needs Manage.'
					: 'Could not read the vocabularies.';
		} finally {
			loading = false;
		}
	}

	onMount(load);

	async function make(event: SubmitEvent) {
		event.preventDefault();
		error = '';
		try {
			const made = await createVocabulary(newKey.trim(), newLabel.trim() || newKey.trim());
			vocabularies = [...vocabularies, made];
			notice = `${made.label} created, and closed to machine tagging until you open it.`;
			newKey = '';
			newLabel = '';
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not create that vocabulary.';
		}
	}

	async function toggleAi(vocabulary: Vocabulary) {
		busy = vocabulary.id;
		error = '';
		try {
			const changed = await setVocabularyAi(vocabulary.id, !vocabulary.ai_taggable);
			vocabularies = vocabularies.map((one) => (one.id === changed.id ? changed : one));
			notice = changed.ai_taggable
				? `${changed.label} is now offered to the model — ${changed.term_count} term${changed.term_count === 1 ? '' : 's'} in every enrichment prompt.`
				: `${changed.label} is closed. Its terms will not be suggested again.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not change that.';
		} finally {
			busy = '';
		}
	}

	async function show(vocabulary: Vocabulary) {
		if (open === vocabulary.id) {
			open = '';
			terms = [];
			return;
		}
		open = vocabulary.id;
		terms = [];
		editing = '';
		merging = '';
		error = '';
		try {
			terms = await vocabularyTerms(vocabulary.id);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not read the terms.';
		}
	}

	/** A comma-separated field into a list. Trimming and de-duplication are the server's job. */
	function split(value: string): string[] {
		return value
			.split(',')
			.map((one) => one.trim())
			.filter(Boolean);
	}

	async function addTerm(vocabulary: Vocabulary, event: SubmitEvent) {
		event.preventDefault();
		busy = vocabulary.id;
		error = '';
		try {
			await addVocabularyTerm(vocabulary.id, {
				slug: termSlug.trim(),
				label: termLabel.trim() || termSlug.trim(),
				synonyms: split(termSynonyms)
			});
			// Refetched rather than appended: the server computes the path and tidies the synonyms, so an
			// appended row would show what was sent instead of what was stored.
			terms = await vocabularyTerms(vocabulary.id);
			vocabularies = await listVocabularies();
			notice = `${termSlug.trim()} added.`;
			termSlug = '';
			termLabel = '';
			termSynonyms = '';
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not add that term.';
		} finally {
			busy = '';
		}
	}

	function edit(term: VocabularyTerm) {
		editing = term.id;
		merging = '';
		draft = {
			label: term.label,
			synonyms: term.synonyms.join(', '),
			threshold: term.ai_threshold
		};
	}

	async function save(vocabulary: Vocabulary, term: VocabularyTerm) {
		busy = term.id;
		error = '';
		try {
			const amended = await amendVocabularyTerm(vocabulary.id, term.id, {
				label: draft.label.trim() || term.label,
				synonyms: split(draft.synonyms),
				ai_threshold: draft.threshold
			});
			terms = terms.map((one) => (one.id === amended.id ? amended : one));
			editing = '';
			// The stored threshold, not the typed one: it is clamped, and saying 1.5 when 1.0 governs would be
			// showing the operator a setting that is not in force.
			notice = `${amended.label} saved, applying above ${amended.ai_threshold}.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not save that term.';
		} finally {
			busy = '';
		}
	}

	async function retire(vocabulary: Vocabulary, term: VocabularyTerm) {
		busy = term.id;
		error = '';
		try {
			const retired = await retireVocabularyTerm(vocabulary.id, term.id);
			terms = terms.map((one) => (one.id === retired.id ? retired : one));
			vocabularies = await listVocabularies();
			// Two sentences, because the zero case is both the commonest retirement and the one where "its 0
			// assets keep it" reads as a glitch. What matters when nothing is tagged is the *other* guarantee:
			// the id still resolves, so a saved search or an integration holding it keeps working.
			notice =
				retired.asset_count === 0
					? `${retired.label} retired. Nothing was tagged with it, and links to its id still resolve.`
					: `${retired.label} retired. Its ${retired.asset_count} asset${retired.asset_count === 1 ? '' : 's'} keep it, and links to it still resolve.`;
		} catch (caught) {
			// A live child is the interesting refusal, and the server's sentence says how many.
			error = caught instanceof Error ? caught.message : 'Could not retire that term.';
		} finally {
			busy = '';
		}
	}

	async function doMerge(vocabulary: Vocabulary, term: VocabularyTerm) {
		if (!mergeInto) return;
		busy = term.id;
		error = '';
		try {
			await mergeVocabularyTerm(vocabulary.id, term.id, mergeInto);
			terms = await vocabularyTerms(vocabulary.id);
			vocabularies = await listVocabularies();
			const target = terms.find((one) => one.id === mergeInto);
			notice = `${term.label} merged into ${target?.label ?? 'the other term'}. Its assets moved and it now resolves there.`;
			merging = '';
			mergeInto = '';
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not merge those terms.';
		} finally {
			busy = '';
		}
	}

	/** The terms a merge may target: live ones, and not the term being merged. */
	const targets = $derived(terms.filter((one) => !one.deprecated_at && one.id !== merging));

	function labelOf(id: string): string {
		return terms.find((one) => one.id === id)?.label ?? id.slice(0, 8);
	}
</script>

<svelte:head><title>Vocabularies · damrs</title></svelte:head>

<div class="space-y-6 p-4">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Tag vocabularies</h1>
		<p class="max-w-2xl text-sm text-muted">
			A vocabulary is the closed set of labels machine tagging scores against. Categories are
			separate — they are where an asset is filed, and a model is never offered them.
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
		<p class="text-sm text-muted">Not connected.</p>
	{:else}
		<form class="flex flex-wrap items-end gap-3 rounded-md border border-line p-3" onsubmit={make}>
			<label class="space-y-1 text-xs">
				<span class="block text-muted">Key</span>
				<input
					bind:value={newKey}
					required
					pattern="[a-z0-9][a-z0-9_\-]*"
					placeholder="moods"
					class="w-40 rounded-md border border-line bg-surface px-2 py-1 font-mono text-sm"
				/>
			</label>
			<label class="space-y-1 text-xs">
				<span class="block text-muted">Label</span>
				<input
					bind:value={newLabel}
					placeholder="Moods"
					class="w-48 rounded-md border border-line bg-surface px-2 py-1 text-sm"
				/>
			</label>
			<button
				type="submit"
				class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised"
			>
				Create
			</button>
			<p class="w-full text-xs text-muted">
				A new vocabulary is closed to machine tagging. Open it once you are happy with its terms.
			</p>
		</form>

		{#if loading}
			<p class="text-sm text-muted">Loading…</p>
		{:else if vocabularies.length === 0}
			<p class="text-sm text-muted">
				No vocabularies yet. Without one, machine tagging has nothing to suggest from.
			</p>
		{/if}

		<ul class="space-y-3">
			{#each vocabularies as vocabulary (vocabulary.id)}
				<li class="space-y-3 rounded-md border border-line p-3">
					<div class="flex flex-wrap items-baseline gap-3">
						<h2 class="text-sm font-semibold tracking-tight">{vocabulary.label}</h2>
						<span class="font-mono text-xs text-muted">{vocabulary.key}</span>
						<span class="text-xs text-muted">
							{vocabulary.term_count}
							{vocabulary.term_count === 1 ? 'term' : 'terms'}
						</span>
						{#if vocabulary.ai_taggable}
							<span class="rounded border border-line px-1.5 py-0.5 text-xs">
								offered to the model
							</span>
						{:else}
							<span class="rounded border border-line px-1.5 py-0.5 text-xs text-muted">
								not offered
							</span>
						{/if}
						<button
							type="button"
							class="ml-auto rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
							aria-expanded={open === vocabulary.id}
							onclick={() => show(vocabulary)}
						>
							{open === vocabulary.id ? 'Hide terms' : 'Terms'}
						</button>
					</div>

					<!--
						Its own row with its own sentence, because it is the one control here that changes what an
						LLM is told about a customer's library.
					-->
					<div class="flex flex-wrap items-center gap-3 border-t border-line pt-2">
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === vocabulary.id}
							onclick={() => toggleAi(vocabulary)}
						>
							{vocabulary.ai_taggable ? 'Close to machine tagging' : 'Open to machine tagging'}
						</button>
						<!--
							The empty case is called out rather than phrased around: an open vocabulary with no live
							terms contributes nothing, and "every one of these 0 terms is in the prompt" reads as a
							glitch. It happens for real — every term retired or merged away leaves exactly this.
						-->
						<p class="text-xs text-muted">
							{#if vocabulary.ai_taggable && vocabulary.term_count === 0}
								Open, but there is nothing live to suggest — every term here is retired. Add one, or
								close it.
							{:else if vocabulary.ai_taggable}
								All {vocabulary.term_count} live term{vocabulary.term_count === 1 ? '' : 's'} sit in the
								prompt of every enrichment call. Closing it stops new suggestions; tags already applied
								stay.
							{:else if vocabulary.term_count === 0}
								Closed, and empty. Add terms before opening it.
							{:else}
								Closed, so nothing here is suggested. Opening it adds {vocabulary.term_count} term{vocabulary.term_count ===
								1
									? ''
									: 's'} to every enrichment prompt, which costs tokens on each call.
							{/if}
						</p>
					</div>

					{#if open === vocabulary.id}
						<div class="space-y-2 border-t border-line pt-2">
							{#if terms.length === 0}
								<p class="text-xs text-muted">No terms yet.</p>
							{:else}
								<ul class="space-y-1">
									{#each terms as term (term.id)}
										<li
											class="flex flex-wrap items-baseline gap-x-3 gap-y-1 rounded border border-line px-2 py-1.5 text-xs {term.deprecated_at
												? 'text-muted'
												: ''}"
										>
											<span class="font-mono">{term.path}</span>
											<span class="font-medium">{term.label}</span>
											{#if term.synonyms.length > 0}
												<span class="text-muted">also: {term.synonyms.join(', ')}</span>
											{/if}
											<span class="text-muted tabular-nums">
												applies above {term.ai_threshold}
											</span>
											{#if term.ai_precision !== null && term.ai_precision !== undefined}
												<!-- Measured from confirmations and rejections, so it is the only number here
												     that is evidence rather than a setting. -->
												<span class="text-muted tabular-nums">
													measured {Math.round(term.ai_precision * 100)}% right
												</span>
											{/if}
											<span class="text-muted tabular-nums">{term.asset_count} tagged</span>

											{#if term.deprecated_at}
												<span class="rounded border border-line px-1.5 py-0.5">
													retired{#if term.superseded_by}, now {labelOf(term.superseded_by)}{/if}
												</span>
											{:else}
												<span class="ml-auto flex gap-1">
													<button
														type="button"
														class="rounded-md border border-line px-2 py-0.5 hover:bg-raised"
														onclick={() => edit(term)}
													>
														Edit
													</button>
													<button
														type="button"
														class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
														disabled={busy === term.id}
														onclick={() => {
															merging = term.id;
															editing = '';
															mergeInto = '';
														}}
													>
														Merge…
													</button>
													<button
														type="button"
														class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
														disabled={busy === term.id}
														onclick={() => retire(vocabulary, term)}
													>
														Retire
													</button>
												</span>
											{/if}

											{#if editing === term.id}
												<div
													class="flex w-full flex-wrap items-end gap-2 border-t border-line pt-2"
												>
													<label class="space-y-1">
														<span class="block text-muted">Label</span>
														<input
															bind:value={draft.label}
															class="w-40 rounded-md border border-line bg-surface px-2 py-1"
														/>
													</label>
													<label class="space-y-1">
														<span class="block text-muted">Synonyms, comma separated</span>
														<input
															bind:value={draft.synonyms}
															class="w-64 rounded-md border border-line bg-surface px-2 py-1"
														/>
													</label>
													<label class="space-y-1">
														<span class="block text-muted">Applies above</span>
														<input
															type="number"
															step="0.05"
															min="0"
															max="1"
															bind:value={draft.threshold}
															class="w-24 rounded-md border border-line bg-surface px-2 py-1 tabular-nums"
														/>
													</label>
													<button
														type="button"
														class="rounded-md border border-line px-2.5 py-1 hover:bg-raised disabled:opacity-50"
														disabled={busy === term.id}
														onclick={() => save(vocabulary, term)}
													>
														Save
													</button>
													<button
														type="button"
														class="px-2 py-1 text-muted underline"
														onclick={() => (editing = '')}
													>
														Cancel
													</button>
													<p class="w-full text-muted">
														The slug stays <span class="font-mono">{term.slug}</span> — it is what the
														model answers with and what an import resolves. Below the threshold a tag
														is suggested for review rather than applied.
													</p>
												</div>
											{/if}

											{#if merging === term.id}
												<div
													class="flex w-full flex-wrap items-end gap-2 border-t border-line pt-2"
												>
													<label class="space-y-1">
														<span class="block text-muted">Merge into</span>
														<select
															bind:value={mergeInto}
															class="rounded-md border border-line bg-surface px-2 py-1"
														>
															<option value="">Choose a term…</option>
															{#each targets as target (target.id)}
																<option value={target.id}>{target.label}</option>
															{/each}
														</select>
													</label>
													<button
														type="button"
														class="rounded-md border border-line px-2.5 py-1 hover:bg-raised disabled:opacity-50"
														disabled={!mergeInto || busy === term.id}
														onclick={() => doMerge(vocabulary, term)}
													>
														Merge
													</button>
													<button
														type="button"
														class="px-2 py-1 text-muted underline"
														onclick={() => (merging = '')}
													>
														Cancel
													</button>
													<p class="w-full text-muted">
														{term.asset_count} asset{term.asset_count === 1 ? '' : 's'} move to the chosen
														term, and <span class="font-mono">{term.slug}</span> retires pointing at it
														— so saved searches and integrations holding its id keep working.
													</p>
												</div>
											{/if}
										</li>
									{/each}
								</ul>
							{/if}

							<form
								class="flex flex-wrap items-end gap-2 border-t border-line pt-2"
								onsubmit={(event) => addTerm(vocabulary, event)}
							>
								<label class="space-y-1 text-xs">
									<span class="block text-muted">Slug</span>
									<input
										bind:value={termSlug}
										required
										pattern="[a-z0-9][a-z0-9_]*"
										class="w-32 rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs"
									/>
								</label>
								<label class="space-y-1 text-xs">
									<span class="block text-muted">Label</span>
									<input
										bind:value={termLabel}
										class="w-40 rounded-md border border-line bg-surface px-2 py-1 text-xs"
									/>
								</label>
								<label class="space-y-1 text-xs">
									<span class="block text-muted">Synonyms, comma separated</span>
									<input
										bind:value={termSynonyms}
										placeholder="cloudy, grey"
										class="w-56 rounded-md border border-line bg-surface px-2 py-1 text-xs"
									/>
								</label>
								<button
									type="submit"
									class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
									disabled={busy === vocabulary.id}
								>
									Add term
								</button>
							</form>
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</div>
