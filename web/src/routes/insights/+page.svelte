<script lang="ts">
	/**
	 * Insights: what the library is used for.
	 *
	 * ## Every number on this page is yours, and the page says so
	 *
	 * §7 says a count is a disclosure, so every figure here is narrowed to the assets this reader can see. Two
	 * people legitimately see different charts. That is stated at the top rather than left to be discovered,
	 * because "Ada says 1,240 and I see 12" is otherwise a bug report — and because it is the reason there is no
	 * library-wide total anywhere on the screen. A total with your share beneath it would tell you exactly how
	 * much you cannot reach.
	 *
	 * ## The chart is drawn from a date spine, so a quiet week looks quiet
	 *
	 * The server returns a row per day including the empty ones. A chart drawn from only the days that had
	 * activity has no holes in it — it draws a straight line across the gap, which is a lie about the shape.
	 *
	 * ## The contributors list is not a performance measure, and says that too
	 *
	 * Ada's upload count is *of the ones you can see*, so it changes with the reader. That is correct for a
	 * disclosure rule and useless as a management number, and the difference is worth one sentence on the page
	 * rather than a conversation later.
	 *
	 * ## SVG, not a charting library
	 *
	 * Six bars a day for thirty days is a `<rect>` each. A dependency would buy interaction nobody asked for and
	 * an accessibility story we would then have to fight; the table beneath the chart is the accessible version,
	 * and it is the same data rather than a summary of it.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import {
		ApiError,
		exportInsights,
		loadInsights,
		type Insights,
		type InsightsReport
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	/** The windows worth offering. A year is the server's own ceiling. */
	const WINDOWS = [
		{ days: 7, label: '7 days' },
		{ days: 30, label: '30 days' },
		{ days: 90, label: '90 days' },
		{ days: 366, label: 'a year' }
	];

	let days = $state(30);
	let data = $state<Insights | null>(null);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');

	async function load() {
		loading = true;
		error = '';
		try {
			data = await loadInsights(days);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the insights.';
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		if (!session.connected) {
			loading = false;
			return;
		}
		void load();
	});

	async function choose(next: number) {
		days = next;
		await load();
	}

	/**
	 * Downloads one report.
	 *
	 * The anchor is created, clicked and revoked here for the same reason the search export does it: a
	 * `download` link needs a blob URL that does not exist until the request has been answered.
	 */
	async function download(report: InsightsReport) {
		error = '';
		notice = '';
		try {
			const blob = await exportInsights(report, days);
			const url = URL.createObjectURL(blob);
			const link = document.createElement('a');
			link.href = url;
			link.download = `${report}.csv`;
			link.click();
			URL.revokeObjectURL(url);
			notice = `Downloaded ${report}.csv.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not export that.';
		}
	}

	/**
	 * One row per kind, each scaled to its own peak.
	 *
	 * Not a stacked chart, and the live page is why. A single day with 160 uploads made every other day a
	 * hairline: the downloads that actually varied — two, three, six, one — were invisible, and any library
	 * that has ever done a bulk import would look like that forever. Stacking also needs a legend nobody
	 * reads, whereas a labelled row says what it is.
	 *
	 * Each row carries its own peak in the label, which is what keeps five different scales honest: rows are
	 * explicitly not comparable to each other, and saying the number is what makes that visible rather than
	 * misleading.
	 */
	const rows = $derived(
		(
			[
				['Uploads', 'uploads', 'var(--color-state-rights-unknown)'],
				['Downloads', 'downloads', 'var(--color-state-rights-allowed)'],
				['Edits', 'edits', 'var(--color-state-rights-expiring)'],
				['Comments', 'comments', 'var(--color-state-neutral)'],
				['Shares', 'shares', 'var(--color-state-rights-denied)']
			] as const
		).map(([label, key, fill]) => {
			const values = (data?.series ?? []).map((day) => day[key]);
			return {
				label,
				key,
				fill,
				values,
				total: values.reduce((sum, value) => sum + value, 0),
				// At least one, so an all-zero row is a flat baseline rather than a division by zero.
				peak: Math.max(1, ...values)
			};
		})
	);

	const totals = $derived({
		uploads: (data?.series ?? []).reduce((sum, day) => sum + day.uploads, 0),
		downloads: (data?.series ?? []).reduce((sum, day) => sum + day.downloads, 0),
		edits: (data?.series ?? []).reduce((sum, day) => sum + day.edits, 0),
		comments: (data?.series ?? []).reduce((sum, day) => sum + day.comments, 0),
		shares: (data?.series ?? []).reduce((sum, day) => sum + day.shares, 0)
	});

	const stored = $derived((data?.by_class ?? []).reduce((sum, one) => sum + one.bytes, 0));

	function size(bytes: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
		let value = bytes;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}

	function stamp(at: string | null | undefined): string {
		return at ? new Date(at).toLocaleDateString() : '—';
	}
</script>

<svelte:head><title>Insights · damrs</title></svelte:head>

<div class="mx-auto max-w-5xl space-y-8 p-8">
	<header class="space-y-1">
		<h1 class="text-2xl font-semibold tracking-tight">Insights</h1>
		<p class="max-w-2xl text-sm text-muted">
			How the library is used. Every number here counts only the assets <em>you</em> can see, so somebody
			with wider access sees larger ones — there is no library-wide total on this page, by design. Downloads
			come from the rights ledger, so one taken through a share link is counted.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>

	{#if !session.connected}
		<p class="text-sm text-muted">
			Not connected. Add an API key in <a class="underline" href={resolve('/settings')}>Settings</a
			>.
		</p>
	{:else}
		<div class="flex flex-wrap items-center gap-2">
			<span class="text-xs text-muted" id="window-label">Window</span>
			<div class="flex gap-1" role="group" aria-labelledby="window-label">
				{#each WINDOWS as option (option.days)}
					<button
						type="button"
						aria-pressed={days === option.days}
						class="rounded-md px-2.5 py-1 text-sm {days === option.days
							? 'bg-surface font-medium'
							: 'text-muted hover:text-fg'}"
						onclick={() => choose(option.days)}
					>
						{option.label}
					</button>
				{/each}
			</div>
		</div>

		{#if loading}
			<p class="text-sm text-muted">Loading…</p>
		{:else if data}
			<section class="space-y-3" aria-labelledby="activity">
				<div class="flex flex-wrap items-baseline gap-x-3">
					<h2 id="activity" class="text-sm font-semibold tracking-tight">Activity</h2>
					<!-- The window the server used, not the one asked for: a request for ten years comes back
					     as a year, and labelling the chart with the request would be wrong. -->
					<span class="text-xs text-muted" data-testid="window">last {data.days} days</span>
					<button
						type="button"
						class="ml-auto rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised"
						onclick={() => download('activity')}
					>
						Export CSV
					</button>
				</div>

				<dl class="flex flex-wrap gap-x-6 gap-y-1 text-sm">
					{#each [['Uploads', totals.uploads], ['Downloads', totals.downloads], ['Edits', totals.edits], ['Comments', totals.comments], ['Shares', totals.shares]] as pair (pair[0])}
						<div>
							<dt class="text-xs text-muted">{pair[0]}</dt>
							<dd class="text-lg font-semibold tabular-nums">
								{Number(pair[1]).toLocaleString()}
							</dd>
						</div>
					{/each}
				</dl>

				<!--
					The chart is decorative and the table below it is the data.

					`aria-hidden` on each SVG, deliberately: a screen reader reading thirty bars as a list of
					numbers is worse than not reading them at all, and every figure is present in the table —
					the same rows, not a summary. That is what makes hiding the picture honest.
				-->
				<ul class="space-y-1" data-testid="sparklines">
					{#each rows as row (row.key)}
						<li class="flex items-center gap-3">
							<span class="w-24 shrink-0 text-xs text-muted">{row.label}</span>
							<svg
								viewBox="0 0 {Math.max(row.values.length, 1) * 4} 20"
								class="h-6 flex-1"
								aria-hidden="true"
								preserveAspectRatio="none"
							>
								{#each row.values as value, index (index)}
									{#if value > 0}
										<rect
											x={index * 4 + 0.5}
											width="3"
											y={20 - (value / row.peak) * 19}
											height={(value / row.peak) * 19}
											fill={row.fill}
										/>
									{:else}
										<!-- A quiet day is a baseline tick rather than nothing, so a gap reads as
										     "nothing happened" rather than as missing data. -->
										<rect
											x={index * 4 + 0.5}
											width="3"
											y="19.5"
											height="0.5"
											fill="var(--color-line)"
										/>
									{/if}
								{/each}
							</svg>
							<!-- The peak, because five rows have five scales and rows are not comparable to each
							     other. Saying the number is what makes that visible rather than misleading. -->
							<span class="w-24 shrink-0 text-right text-xs text-muted tabular-nums">
								peak {row.peak.toLocaleString()}/day
							</span>
						</li>
					{/each}
				</ul>

				<details>
					<summary class="cursor-pointer text-xs text-muted">
						The same figures as a table ({data.series.length} days)
					</summary>
					<div class="mt-2 overflow-x-auto">
						<table class="w-full text-left text-xs" data-testid="series">
							<caption class="sr-only">
								Activity per day for the last {data.days} days, counting only assets you can see
							</caption>
							<thead>
								<tr class="text-muted">
									<th scope="col" class="py-1 pr-3 font-medium">Day</th>
									<th scope="col" class="py-1 pr-3 font-medium">Uploads</th>
									<th scope="col" class="py-1 pr-3 font-medium">Downloads</th>
									<th scope="col" class="py-1 pr-3 font-medium">Edits</th>
									<th scope="col" class="py-1 pr-3 font-medium">Comments</th>
									<th scope="col" class="py-1 font-medium">Shares</th>
								</tr>
							</thead>
							<tbody class="tabular-nums">
								{#each data.series as point (point.day)}
									<tr class="border-t border-line">
										<th scope="row" class="py-1 pr-3 font-normal">{point.day}</th>
										<td class="py-1 pr-3">{point.uploads}</td>
										<td class="py-1 pr-3">{point.downloads}</td>
										<td class="py-1 pr-3">{point.edits}</td>
										<td class="py-1 pr-3">{point.comments}</td>
										<td class="py-1">{point.shares}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				</details>
			</section>

			<section class="space-y-2" aria-labelledby="storage">
				<div class="flex flex-wrap items-baseline gap-x-3">
					<h2 id="storage" class="text-sm font-semibold tracking-tight">Storage</h2>
					<span class="text-xs text-muted">{size(stored)} in what you can see</span>
					<button
						type="button"
						class="ml-auto rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised"
						onclick={() => download('storage')}
					>
						Export CSV
					</button>
				</div>
				{#if data.by_class.length === 0}
					<p class="text-sm text-muted">Nothing stored that you can see.</p>
				{:else}
					<ul class="space-y-1" data-testid="by-class">
						{#each data.by_class as row (row.class)}
							<li class="flex flex-wrap items-baseline gap-x-3 text-sm">
								<span class="w-20 capitalize">{row.class}</span>
								<span class="tabular-nums">{size(row.bytes)}</span>
								<span class="text-xs text-muted"
									>{row.assets.toLocaleString()}
									{row.assets === 1 ? 'asset' : 'assets'}</span
								>
								<!-- A proportion bar, `aria-hidden` because the two numbers beside it are the
								     information and a percentage read aloud adds nothing. -->
								<span
									aria-hidden="true"
									class="h-1.5 flex-1 overflow-hidden rounded-full bg-surface"
								>
									<span
										class="block h-full rounded-full bg-accent"
										style="width: {stored > 0 ? (row.bytes / stored) * 100 : 0}%"
									></span>
								</span>
							</li>
						{/each}
					</ul>
				{/if}
			</section>

			<div class="grid gap-8 md:grid-cols-2">
				<section class="space-y-2" aria-labelledby="most">
					<div class="flex flex-wrap items-baseline gap-x-3">
						<h2 id="most" class="text-sm font-semibold tracking-tight">Most downloaded</h2>
						<button
							type="button"
							class="ml-auto rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised"
							onclick={() => download('most-downloaded')}
						>
							Export CSV
						</button>
					</div>
					{#if data.most_downloaded.length === 0}
						<p class="text-sm text-muted">Nothing you can see was downloaded in this window.</p>
					{:else}
						<ol class="space-y-1" data-testid="most-downloaded">
							{#each data.most_downloaded as row (row.asset_id)}
								<li class="flex items-baseline gap-2 text-sm">
									<span class="font-semibold tabular-nums">{row.count}</span>
									<span class="min-w-0 flex-1 truncate" title={row.filename}>{row.filename}</span>
									<span class="text-xs text-muted">{stamp(row.last_at)}</span>
								</li>
							{/each}
						</ol>
					{/if}
				</section>

				<section class="space-y-2" aria-labelledby="unused">
					<div class="flex flex-wrap items-baseline gap-x-3">
						<h2 id="unused" class="text-sm font-semibold tracking-tight">Never downloaded</h2>
						<button
							type="button"
							class="ml-auto rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised"
							onclick={() => download('never-downloaded')}
						>
							Export CSV
						</button>
					</div>
					<!-- Ever, not "in this window", and oldest first — because age is the whole signal. An
					     asset uploaded yesterday and not yet downloaded is not a finding.

					     And the total, not just the page. Twenty rows here read as "we have twenty unused
					     assets"; on the dev library that was twenty of a far larger number, which is the
					     difference between a tidy-up and a storage problem. A most-downloaded top-20 explains
					     its own cap and this does not, so this one says how many there are. -->
					<p class="text-xs text-muted">
						Never taken by anybody, ever — not just in this window. Oldest first, because that is
						what makes it worth looking at.
						{#if data.never_downloaded_total > data.never_downloaded.length}
							Showing the {data.never_downloaded.length} oldest of
							<strong>{data.never_downloaded_total.toLocaleString()}</strong>.
						{/if}
					</p>
					{#if data.never_downloaded.length === 0}
						<p class="text-sm text-muted">
							Everything you can see has been downloaded at least once.
						</p>
					{:else}
						<ul class="space-y-1" data-testid="never-downloaded">
							{#each data.never_downloaded as row (row.asset_id)}
								<li class="truncate text-sm" title={row.filename}>{row.filename}</li>
							{/each}
						</ul>
					{/if}
				</section>
			</div>

			<section class="space-y-2" aria-labelledby="people">
				<div class="flex flex-wrap items-baseline gap-x-3">
					<h2 id="people" class="text-sm font-semibold tracking-tight">Contributors</h2>
					<button
						type="button"
						class="ml-auto rounded-md border border-line px-2 py-0.5 text-xs hover:bg-raised"
						onclick={() => download('contributors')}
					>
						Export CSV
					</button>
				</div>
				<!--
					Said out loud, because somebody will otherwise use it as one.

					These counts are scoped to the reader, so the same person's upload count differs for
					everybody who looks at it. That is correct for a disclosure rule and meaningless as a
					measure of anyone's work.
				-->
				<p class="max-w-2xl text-xs text-muted">
					Counted over the assets you can see, so the same person's numbers are different for every
					reader — which is why this is not a measure of anybody's work. Downloads are deliberately
					absent: who took a particular asset is answered on that asset, where the question belongs.
				</p>
				{#if data.contributors.length === 0}
					<p class="text-sm text-muted">No activity by anybody in this window.</p>
				{:else}
					<table class="w-full text-left text-sm" data-testid="contributors">
						<thead>
							<tr class="text-xs text-muted">
								<th scope="col" class="py-1 pr-3 font-medium">Person</th>
								<th scope="col" class="py-1 pr-3 font-medium">Uploads</th>
								<th scope="col" class="py-1 pr-3 font-medium">Edits</th>
								<th scope="col" class="py-1 font-medium">Comments</th>
							</tr>
						</thead>
						<tbody class="tabular-nums">
							{#each data.contributors as row (row.person.id)}
								<tr class="border-t border-line">
									<th scope="row" class="py-1 pr-3 font-normal">
										{row.person.name}
										<!--
											Only when it adds something. `display_name` falls back to the email
											when nobody set one, and the live page then read
											"ada@example.com ada@example.com" on every row. The email is here
											because two colleagues can share a display name — which is not a
											reason to print the same string twice.
										-->
										{#if row.person.email && row.person.email !== row.person.name}
											<span class="text-xs text-muted">{row.person.email}</span>
										{/if}
									</th>
									<td class="py-1 pr-3">{row.uploads}</td>
									<td class="py-1 pr-3">{row.edits}</td>
									<td class="py-1">{row.comments}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</section>
		{/if}
	{/if}
</div>
