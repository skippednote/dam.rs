<!--
	The conversation about one asset.

	## The compose box states the consequence, not the setting

	"Everyone who can see this asset" and "Only you and the people you choose" — rather than a switch labelled
	public/private and left to be inferred. Getting this wrong is not a cosmetic error: somebody who believes a note
	is private and posts it publicly cannot take it back, and the words that make the difference are two lines of
	copy.

	While the private option is chosen and nobody is named, the send control is disabled and says why. The server
	refuses that combination anyway — a note only its author can read is one that failed silently — but a form that
	lets you write three paragraphs before refusing them is a form that wasted your time on purpose.

	## The words and the status are separate controls

	They carry different rights: the words are their author's, the status is any reader's. The API refuses a request
	naming both, and the UI never offers to send both, so that refusal is unreachable from here rather than
	something a person can trip over.

	## Replies are one level deep

	The server refuses a reply to a reply, so there is no reply control on a reply. An affordance that exists only
	to be refused is worse than no affordance.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		ApiError,
		amendComment,
		deleteComment,
		listComments,
		listPeople,
		postComment,
		whoAmI,
		type Comment,
		type Person
	} from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	/**
	 * Who is reading, resolved here rather than threaded in from the page.
	 *
	 * The panel is the only thing that needs it — to know which comments it may offer to edit — and asking the
	 * page to fetch and pass it would put an identity lookup in a component that has no other use for one.
	 */
	let me = $state<string | undefined>(undefined);

	let comments = $state<Comment[]>([]);
	let people = $state<Person[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let loaded = $state(false);

	/**
	 * The compose box. `chosen` is the deliberate option, so `everyone` is the default.
	 *
	 * The audience is a named state rather than a boolean. `bind:group` with `value={true}` / `value={false}` does
	 * *not* honour a boolean initial value — the radios rendered with the public one checked regardless, so the
	 * control looked right only because the default happened to be public. Radios are string-valued in the DOM, so
	 * saying that outright removes the coercion question and reads closer to the copy besides.
	 */
	let draft = $state({
		body: '',
		audience: 'everyone' as 'everyone' | 'chosen',
		recipients: [] as string[]
	});

	/** Whether the draft is private. Derived, so there is one source of truth for the audience. */
	const privately = $derived(draft.audience === 'chosen');

	/**
	 * Everybody the caller could address a private comment to — which is everyone but themselves.
	 *
	 * Empty in a tenant with one member, and that case has to be said rather than shown as an empty box beside an
	 * unsatisfiable requirement. Found by driving the real thing on a dev tenant with a single identity: the option
	 * was offered, the picker was empty, and Send stayed disabled with nothing explaining why.
	 */
	const addressable = $derived(people.filter((person) => person.id !== me));
	/** Which comment a reply is being written to, if any. */
	let replyingTo = $state<string | null>(null);
	let replyBody = $state('');
	/** Which comment is being edited, and its working text. */
	let editing = $state<string | null>(null);
	let editBody = $state('');
	let confirmingRemoval = $state<string | null>(null);

	const STATUSES = [
		['open', 'Open'],
		['changes_requested', 'Changes requested'],
		['approved', 'Approved'],
		['resolved', 'Resolved']
	] as const;

	/** Top-level comments, in the order the server returned them. */
	const threads = $derived(comments.filter((comment) => !comment.parent_id));

	function repliesTo(id: string): Comment[] {
		return comments.filter((comment) => comment.parent_id === id);
	}

	/** Whether the compose box can be sent. See the component docs on why this is disabled rather than refused. */
	const sendable = $derived(
		draft.body.trim().length > 0 && (!privately || draft.recipients.length > 0)
	);

	async function load() {
		error = '';
		try {
			// Settled independently: a failure reading the roster must not empty the thread, and a failure
			// resolving the reader must not either — it only costs the Edit control. The same lesson as the
			// auto-import picker, which lost its options to an unrelated 500.
			const [found, roster, viewer] = await Promise.allSettled([
				listComments(assetId),
				listPeople(),
				whoAmI()
			]);
			if (found.status === 'fulfilled') comments = found.value;
			if (roster.status === 'fulfilled') people = roster.value;
			if (viewer.status === 'fulfilled') me = viewer.value.id;
			if (found.status === 'rejected') {
				error =
					found.reason instanceof ApiError
						? found.reason.message
						: 'Could not load the conversation.';
			}
		} finally {
			loaded = true;
		}
	}

	/** The asset this panel is showing, so switching selection resets the drafts rather than carrying them over. */
	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		// The first run has nothing to forget, and skipping it matters for more than efficiency: without this the
		// reset below overwrote the declared defaults on mount, so the `$state` initialiser was dead code and the
		// compose box had *two* defaults that could disagree. Mutation testing found it — flipping the declared
		// default changed nothing on screen.
		if (shownFor === null) {
			shownFor = id;
			void load();
			return;
		}
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			comments = [];
			people = [];
			loaded = false;
			error = '';
			notice = '';
			draft = { body: '', audience: 'everyone', recipients: [] };
			replyingTo = null;
			editing = null;
			confirmingRemoval = null;
			void load();
		});
	});

	async function run(work: () => Promise<string>) {
		busy = true;
		error = '';
		try {
			notice = await work();
			await load();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'That could not be saved.';
		} finally {
			busy = false;
		}
	}

	function send(event: SubmitEvent) {
		event.preventDefault();
		if (!sendable) return;
		void run(async () => {
			const wasPrivate = privately;
			await postComment(assetId, {
				body: draft.body.trim(),
				visibility: wasPrivate ? 'private' : 'public',
				// Dropped when the audience is everyone: a public comment routed to people would notify them about
				// something they can already see, and it would keep a decision the person visibly reversed.
				recipients: wasPrivate ? draft.recipients : []
			});
			draft = { body: '', audience: 'everyone', recipients: [] };
			return wasPrivate ? 'Posted, visible only to the people you named.' : 'Posted.';
		});
	}

	function reply(event: SubmitEvent, parentId: string) {
		event.preventDefault();
		const body = replyBody.trim();
		if (!body) return;
		void run(async () => {
			// A reply inherits nothing: it is public unless somebody says otherwise, like any other comment.
			await postComment(assetId, { body, parent_id: parentId });
			replyingTo = null;
			replyBody = '';
			return 'Reply posted.';
		});
	}

	function saveEdit(event: SubmitEvent, id: string) {
		event.preventDefault();
		const body = editBody.trim();
		if (!body) return;
		void run(async () => {
			await amendComment(id, { body });
			editing = null;
			return 'Comment updated. It is marked as edited.';
		});
	}

	function moveStatus(comment: Comment, status: Comment['status']) {
		void run(async () => {
			const after = await amendComment(comment.id, { status });
			const label = STATUSES.find(([value]) => value === after.status)?.[1] ?? after.status;
			return `Marked ${label.toLowerCase()}.`;
		});
	}

	function remove(comment: Comment) {
		confirmingRemoval = null;
		void run(async () => {
			await deleteComment(comment.id);
			return repliesTo(comment.id).length > 0
				? 'Comment deleted, along with the replies to it.'
				: 'Comment deleted.';
		});
	}

	function when(iso: string): string {
		return new Date(iso).toLocaleString();
	}

	function nameOf(id: string): string {
		return people.find((person) => person.id === id)?.name ?? id;
	}
</script>

<section class="space-y-3" aria-label="Comments">
	<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">
		Comments{#if comments.length > 0}&nbsp;({comments.length}){/if}
	</h3>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>

	{#if !loaded}
		<p class="text-xs text-muted">Loading…</p>
	{:else if threads.length === 0}
		<p class="text-xs text-muted">No comments yet.</p>
	{:else}
		<ul class="space-y-3">
			{#each threads as comment (comment.id)}
				<li class="space-y-2 rounded-md border border-line p-2">
					<div class="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-xs">
						<span class="font-medium">{comment.author.name}</span>
						<time datetime={comment.created_at} class="text-muted">{when(comment.created_at)}</time>
						{#if comment.edited_at}
							<!-- Said, not hidden: whoever replied to this read different words. -->
							<span class="text-muted">· edited</span>
						{/if}
						{#if comment.visibility === 'private'}
							<span
								class="rounded bg-state-rights-denied/18 px-1.5 py-0.5 font-medium text-state-rights-denied-fg"
							>
								private
							</span>
						{/if}
						{#if comment.status !== 'open'}
							<span class="rounded bg-raised px-1.5 py-0.5 text-muted">
								{STATUSES.find(([value]) => value === comment.status)?.[1] ?? comment.status}
								{#if comment.status_by}· by {comment.status_by.name}{/if}
							</span>
						{/if}
					</div>

					{#if comment.visibility === 'private' && comment.recipients.length > 0}
						<p class="text-[11px] text-muted">
							Visible to {comment.author.name} and {comment.recipients
								.map((person) => person.name)
								.join(', ')}
						</p>
					{/if}

					{#if editing === comment.id}
						<form class="space-y-2" onsubmit={(event) => saveEdit(event, comment.id)}>
							<label class="block">
								<span class="sr-only">Edit comment</span>
								<textarea
									bind:value={editBody}
									rows="3"
									class="w-full rounded-md border border-line bg-bg p-2 text-xs"></textarea>
							</label>
							<span class="flex items-center gap-2">
								<button
									type="submit"
									class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
									disabled={busy || editBody.trim().length === 0}
								>
									Save
								</button>
								<button type="button" class="text-xs underline" onclick={() => (editing = null)}>
									Cancel
								</button>
							</span>
						</form>
					{:else}
						<p class="text-xs whitespace-pre-wrap">{comment.body}</p>
					{/if}

					<div class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
						<button
							type="button"
							class="underline"
							onclick={() => {
								replyingTo = replyingTo === comment.id ? null : comment.id;
								replyBody = '';
							}}
						>
							Reply
						</button>

						<!--
							Any reader may move a status: `approved` is somebody else's verdict on what the comment
							asked for, so a control only its author could use could never mean approval.
						-->
						<label class="flex items-center gap-1">
							<span class="sr-only">Status of {comment.author.name}'s comment</span>
							<select
								class="rounded border border-line bg-bg px-1 py-0.5 text-xs"
								value={comment.status}
								disabled={busy}
								onchange={(event) =>
									moveStatus(comment, event.currentTarget.value as Comment['status'])}
							>
								{#each STATUSES as [value, label] (value)}
									<option {value}>{label}</option>
								{/each}
							</select>
						</label>

						{#if me && comment.author.id === me}
							<!-- Offered only to the author, because the server refuses anyone else — an affordance
							     that exists to be refused is worse than none. -->
							<button
								type="button"
								class="underline"
								onclick={() => {
									editing = comment.id;
									editBody = comment.body;
								}}
							>
								Edit
							</button>
							<button
								type="button"
								class="underline"
								onclick={() =>
									(confirmingRemoval = confirmingRemoval === comment.id ? null : comment.id)}
							>
								Delete
							</button>
						{/if}
					</div>

					{#if confirmingRemoval === comment.id}
						<div class="rounded-md bg-surface p-2 text-[11px]">
							<p>
								{#if repliesTo(comment.id).length > 0}
									Deleting this also deletes the {repliesTo(comment.id).length} repl{repliesTo(
										comment.id
									).length === 1
										? 'y'
										: 'ies'} to it. A reply to a question that no longer exists reads as corruption.
								{:else}
									This cannot be undone.
								{/if}
							</p>
							<span class="mt-1 flex items-center gap-2">
								<button
									type="button"
									class="rounded-md bg-state-rights-denied px-2 py-0.5 font-medium text-state-rights-denied-fg disabled:opacity-50"
									disabled={busy}
									onclick={() => remove(comment)}
								>
									Delete
								</button>
								<button type="button" class="underline" onclick={() => (confirmingRemoval = null)}>
									Cancel
								</button>
							</span>
						</div>
					{/if}

					{#if repliesTo(comment.id).length > 0}
						<ul class="space-y-2 border-l border-line pl-3">
							{#each repliesTo(comment.id) as child (child.id)}
								<li class="space-y-1">
									<div class="flex flex-wrap items-baseline gap-x-2 text-xs">
										<span class="font-medium">{child.author.name}</span>
										<time datetime={child.created_at} class="text-muted">
											{when(child.created_at)}
										</time>
										{#if child.edited_at}<span class="text-muted">· edited</span>{/if}
									</div>
									<p class="text-xs whitespace-pre-wrap">{child.body}</p>
									<!-- No reply control on a reply: threads are one level deep and the server
									     refuses a deeper one. -->
								</li>
							{/each}
						</ul>
					{/if}

					{#if replyingTo === comment.id}
						<form class="space-y-2" onsubmit={(event) => reply(event, comment.id)}>
							<label class="block">
								<span class="sr-only">Reply to {comment.author.name}</span>
								<textarea
									bind:value={replyBody}
									rows="2"
									placeholder="Reply…"
									class="w-full rounded-md border border-line bg-bg p-2 text-xs"></textarea>
							</label>
							<span class="flex items-center gap-2">
								<button
									type="submit"
									class="rounded-md bg-accent px-2.5 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
									disabled={busy || replyBody.trim().length === 0}
								>
									Post reply
								</button>
								<button type="button" class="text-xs underline" onclick={() => (replyingTo = null)}>
									Cancel
								</button>
							</span>
						</form>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}

	<form class="space-y-2 rounded-md bg-surface p-2" onsubmit={send}>
		<label class="block">
			<span class="sr-only">Add a comment</span>
			<textarea
				bind:value={draft.body}
				rows="3"
				placeholder="Add a comment…"
				class="w-full rounded-md border border-line bg-bg p-2 text-xs"></textarea>
		</label>

		<!--
			The consequence, not the setting. A switch labelled "public/private" leaves the reader to infer what
			either means, and somebody who infers wrongly cannot take the words back.
		-->
		<fieldset class="space-y-1 text-xs">
			<legend class="sr-only">Who can see this comment</legend>
			<!--
				String values, bound as a group. Two earlier shapes were both wrong and both *looked* right: an
				attribute `checked={…}` sets `defaultChecked` and lets the property drift, and `bind:group` over
				booleans ignores the initial value entirely. Either way, flipping the default in the source changed
				nothing on screen — visible only because a mutation flipped it and no test noticed.
			-->
			<label class="flex items-start gap-2">
				<input
					type="radio"
					name="visibility-{assetId}"
					value="everyone"
					bind:group={draft.audience}
				/>
				<span>Everyone who can see this asset</span>
			</label>
			<label class="flex items-start gap-2">
				<input
					type="radio"
					name="visibility-{assetId}"
					value="chosen"
					bind:group={draft.audience}
				/>
				<span>Only you and the people you choose</span>
			</label>
		</fieldset>

		{#if privately && addressable.length === 0}
			<p class="text-xs text-muted">
				There is nobody else here yet, so a private comment would have no audience. Invite somebody,
				or post it where everyone who can see this asset will.
			</p>
		{:else if privately}
			<label class="block text-xs">
				<span class="text-muted">Who can see it</span>
				<select
					multiple
					size="4"
					class="mt-1 w-full rounded-md border border-line bg-bg p-1 text-xs"
					onchange={(event) => {
						draft.recipients = [...event.currentTarget.selectedOptions].map(
							(option) => option.value
						);
					}}
				>
					{#each addressable as person (person.id)}
						<!-- The email alongside the name, because two colleagues can share a name and choosing the
						     wrong one misroutes a note meant to be private. -->
						<option value={person.id}>{person.name} · {person.email}</option>
					{/each}
				</select>
			</label>
			{#if draft.recipients.length === 0}
				<!--
					Said before sending rather than refused after. The server refuses a private comment addressed to
					nobody, and a form that lets you write three paragraphs first wasted your time on purpose.
				-->
				<p class="text-xs text-state-rights-denied-fg">
					Choose at least one person, or nobody but you will ever see this.
				</p>
			{:else}
				<p class="text-[11px] text-muted">
					Only {draft.recipients.map(nameOf).join(', ')} will see this.
				</p>
			{/if}
		{/if}

		<button
			type="submit"
			class="rounded-md bg-accent px-3 py-1 text-xs font-medium text-accent-fg disabled:opacity-50"
			disabled={busy || !sendable}
		>
			{privately ? 'Post privately' : 'Post'}
		</button>
	</form>
</section>
