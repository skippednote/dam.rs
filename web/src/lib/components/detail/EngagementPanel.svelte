<!--
	What this asset means to the people using it: a rating, a favourite, a watch.

	## The rating is a radio group, not five buttons

	Five stars are five values of one thing, and that is what a radio group is. Buttons would each be a separate
	control to a screen reader — "button, 1. button, 2. button, 3." — with nothing saying they are alternatives or
	which one is chosen. A radio group announces "3 of 5 stars, selected" and arrows between the options for free.

	The visible stars are drawn from the *average*, and the selected radio is the caller's own rating. Those are
	different facts and the panel says so in words, because a widget that showed one number could only ever be
	lying about the other.

	## Clearing is a separate control, because there is no zero star

	The API has no zero: "no opinion" and "thinks it is bad" must not share a representation, or an average
	silently counts absences. So clearing is its own button, offered only when there is something to clear —
	rather than a sixth star meaning "none", which is exactly the conflation the model avoids.

	## Favourite and watch are toggles that say what they do

	`aria-pressed`, so the state is announced rather than implied by colour, and a label that describes the
	*consequence* — "watching" means you will be told when it changes, which is not obvious from an eye icon.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		ApiError,
		clearRating,
		setFavourite,
		setRating,
		setWatch,
		type Engagement
	} from '$lib/api/client';

	let {
		assetId,
		/** The engagement the detail payload already carried, so the panel draws immediately. */
		initial,
		onchanged
	}: {
		assetId: string;
		/**
		 * The engagement the detail payload carried.
		 *
		 * Optional only for robustness. The API always sends it, so an absent value means something upstream is
		 * out of step — and the panel hides itself rather than guessing, because "absent" and "nobody has rated
		 * this" are different facts. Hiding also keeps the blast radius proportionate: reading a field off an
		 * undefined object took the *whole* detail panel down with it, metadata editor and all.
		 */
		initial?: Engagement;
		/** So a grid can redraw its star without refetching the page. */
		onchanged?: (after: Engagement) => void;
	} = $props();

	/**
	 * The server's answer to whatever this panel last did, or nothing yet.
	 *
	 * Derived against the prop rather than copied from it: a `$state` seeded from `initial` captures only the
	 * first value, so selecting a second asset would keep drawing the first one's stars until something else
	 * wrote to it. This way the payload is the source until the panel changes something, and the effect below
	 * only has to forget the override.
	 */
	let override = $state<Engagement | null>(null);
	const current = $derived(override ?? initial);

	let error = $state('');
	let busy = $state(false);
	/** The last thing that happened, announced rather than only drawn. */
	let notice = $state('');

	const STARS = [1, 2, 3, 4, 5];

	/**
	 * The asset this panel's own state belongs to, once the effect below has seen one.
	 *
	 * A plain variable rather than `$state`, deliberately: it is bookkeeping for that effect and nothing renders
	 * from it, so making it reactive would only give the effect a second reason to run. `null` until the first run
	 * rather than seeded from the prop, because reading a prop in an initialiser captures only its first value —
	 * true here and harmless, but the compiler cannot tell the difference and the warning would be noise forever.
	 */
	let shownFor: string | null = null;

	/**
	 * Forgets this panel's own state when the selected asset changes — and *only* then.
	 *
	 * The id is compared rather than merely read. A prop read is a signal subscription, so replacing the asset
	 * object at all re-ran this effect even though the id was identical — and the page does replace it, because a
	 * favourite from the grid patches the open asset. The result was that the server's answer and the sentence
	 * describing it were both thrown away immediately after arriving.
	 *
	 * Everything else the body writes it also reads, hence `untrack` — the trap the category panel and the bulk
	 * bar both hit.
	 */
	$effect(() => {
		const id = assetId;
		// The first run has nothing to forget: the panel is drawing the asset it was created for.
		if (shownFor === null) {
			shownFor = id;
			return;
		}
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			override = null;
			error = '';
			notice = '';
		});
	});

	async function run(work: () => Promise<Engagement>, said: (after: Engagement) => string) {
		busy = true;
		error = '';
		try {
			// From the server's answer, not an optimistic guess: the average moved because of this request, and
			// a guess could disagree with it.
			override = await work();
			notice = said(override);
			onchanged?.(override);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'That change could not be saved.';
		} finally {
			busy = false;
		}
	}

	function rate(stars: number) {
		void run(
			() => setRating(assetId, stars),
			(after) =>
				`Rated ${stars} of 5. The average is now ${average(after)} from ${after.rating_count} ${
					after.rating_count === 1 ? 'rating' : 'ratings'
				}.`
		);
	}

	function unrate() {
		void run(
			() => clearRating(assetId),
			(after) =>
				after.rating_count === 0
					? 'Your rating is removed. Nobody has rated this.'
					: `Your rating is removed. The average is now ${average(after)}.`
		);
	}

	function favourite() {
		if (!current) return;
		const next = !current.is_favourite;
		void run(
			() => setFavourite(assetId, next),
			(after) =>
				after.is_favourite ? 'Added to your favourites.' : 'Removed from your favourites.'
		);
	}

	function watch() {
		if (!current) return;
		const next = !current.is_watched;
		void run(
			() => setWatch(assetId, next),
			(after) =>
				after.is_watched
					? 'Watching. You will be told when this changes.'
					: 'No longer watching this.'
		);
	}

	/** The average to one decimal, or a dash when nobody has rated it. */
	function average(of: Engagement): string {
		return of.average_stars === null || of.average_stars === undefined
			? '—'
			: of.average_stars.toFixed(1);
	}

	/**
	 * How much of star `n` the average fills, 0 to 1.
	 *
	 * Partial fill rather than rounding, because an average of 3.4 and one of 3.5 are different numbers and a
	 * widget that drew both as "3 stars" would discard the distinction it exists to show.
	 */
	function fill(n: number): number {
		const value = current?.average_stars ?? 0;
		return Math.min(1, Math.max(0, value - (n - 1)));
	}
</script>

<!-- Nothing at all when there is no engagement to draw: see the note on `initial`. -->
{#if current}
	<section class="space-y-2" aria-label="Ratings and favourites">
		<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">Engagement</h3>

		{#if error}
			<p
				role="alert"
				class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
			>
				{error}
			</p>
		{/if}

		<!--
		One live region for every change in this panel. `polite` so it waits for a pause rather than interrupting,
		and it holds the sentence the server's answer produced — "the average is now 3.5 from 4 ratings" is the
		part a person cannot see from the stars alone.
	-->
		<p role="status" aria-live="polite" class="text-xs text-muted">
			{#if notice}
				{notice}
			{:else if current.rating_count === 0}
				Not yet rated.
			{:else}
				Average {average(current)} from {current.rating_count}
				{current.rating_count === 1 ? 'rating' : 'ratings'}.
			{/if}
		</p>

		<!--
		The average, drawn. `aria-hidden` because the sentence above already says it in words — a screen reader
		reading five partially-filled stars would announce nothing useful.
	-->
		<div class="flex items-center gap-1" aria-hidden="true">
			{#each STARS as n (n)}
				<span class="relative inline-block h-4 w-4 text-muted">
					<span class="absolute inset-0">★</span>
					<span
						class="absolute inset-0 overflow-hidden text-accent"
						data-testid="star-fill"
						style="width: {fill(n) * 100}%"
					>
						★
					</span>
				</span>
			{/each}
			<span class="ml-1 text-xs text-muted tabular-nums">{average(current)}</span>
		</div>

		<!--
		The caller's own rating, as a radio group: five values of one thing. Buttons would be five unrelated
		controls to a screen reader, with nothing saying which is chosen or that they are alternatives.
	-->
		<fieldset class="flex flex-wrap items-center gap-2" disabled={busy}>
			<legend class="text-xs text-muted">Your rating</legend>
			{#each STARS as n (n)}
				<label class="flex cursor-pointer items-center gap-1 text-xs">
					<input
						type="radio"
						name="rating-{assetId}"
						value={n}
						checked={current.my_stars === n}
						onchange={() => rate(n)}
					/>
					{n}
				</label>
			{/each}
			{#if current.my_stars !== null && current.my_stars !== undefined}
				<!--
				Only when there is something to clear, and separate from the stars: there is no zero star, because
				"no opinion" and "thinks it is bad" must not share a representation.
			-->
				<button type="button" class="text-xs underline" disabled={busy} onclick={unrate}>
					Clear
				</button>
			{/if}
		</fieldset>

		<div class="flex flex-wrap items-center gap-2">
			<button
				type="button"
				aria-pressed={current.is_favourite}
				disabled={busy}
				onclick={favourite}
				class="rounded-md border border-line px-2.5 py-1 text-xs disabled:opacity-50
			       aria-pressed:bg-accent aria-pressed:text-accent-fg"
			>
				{current.is_favourite ? '★ Favourite' : '☆ Favourite'}
			</button>
			<button
				type="button"
				aria-pressed={current.is_watched}
				disabled={busy}
				onclick={watch}
				class="rounded-md border border-line px-2.5 py-1 text-xs disabled:opacity-50
			       aria-pressed:bg-accent aria-pressed:text-accent-fg"
			>
				{current.is_watched ? 'Watching' : 'Watch'}
			</button>
			{#if current.favourite_count > 0}
				<!--
				A count of people, never a list of them. Nothing on this screen needs to know *who*, and "seven
				people favourited this, and here they are" is a different disclosure from "seven people did".
			-->
				<span class="text-xs text-muted">
					{current.favourite_count}
					{current.favourite_count === 1 ? 'person has' : 'people have'} favourited this
				</span>
			{/if}
		</div>
		<p class="text-xs text-muted">
			Watching tells you when this asset changes. Nobody is told how many people are watching.
		</p>
	</section>
{/if}
