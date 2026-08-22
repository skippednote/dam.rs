<script lang="ts">
	/**
	 * Storage administration: what the tiering rules would do, and what they have done.
	 *
	 * ## The plan is the screen, not the run button
	 *
	 * A lifecycle policy moves originals into classes that take between a minute and 48 hours to read back,
	 * and bills a 30-to-180-day minimum for the privilege. So the affordance that matters is *reading what
	 * would happen*, which is why every row leads with a Plan button and the sweep is the smaller control at
	 * the top. `dam_store::lifecycle`'s own docs put it best: a plan can be read, diffed and approved before
	 * terabytes of a customer's masters go somewhere they cannot be read back from for two days.
	 *
	 * ## Dry run is stated on every row, because it is the whole story
	 *
	 * A policy that has never been taken off dry run has never moved anything. That is the default and it is
	 * the most important fact about a rule, so it sits on the row rather than behind an edit form.
	 *
	 * ## The skips are grouped, and that is the point
	 *
	 * A plan over a real library is thousands of lines, almost all of them skips. Ungrouped they are
	 * unreadable; grouped by reason they answer the only question anybody asks — "why did nothing happen?" —
	 * in one glance, with the pinned ones spelled out because those are the ones somebody may need to act on.
	 */
	import { onMount } from 'svelte';
	import {
		ApiError,
		listLifecyclePolicies,
		planLifecyclePolicy,
		runLifecycleSweep,
		type LifecyclePlan,
		type LifecyclePolicy
	} from '$lib/api/client';

	let policies = $state<LifecyclePolicy[]>([]);
	let plans = $state<Record<string, LifecyclePlan>>({});
	let planning = $state('');
	let error = $state('');
	let queued = $state('');

	async function load() {
		error = '';
		try {
			policies = await listLifecyclePolicies();
		} catch (caught) {
			error =
				caught instanceof ApiError
					? caught.message
					: 'Could not read the tiering rules. Is the key a tenant administrator’s?';
		}
	}

	onMount(load);

	async function plan(id: string) {
		planning = id;
		error = '';
		try {
			plans = { ...plans, [id]: await planLifecyclePolicy(id) };
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not plan that policy.';
		} finally {
			planning = '';
		}
	}

	async function sweep() {
		error = '';
		queued = '';
		try {
			const run = await runLifecycleSweep();
			// The dry-run count is restated here because "I pressed run and nothing moved" is otherwise a
			// support ticket rather than a policy that is doing exactly what it says.
			queued =
				run.policies_in_dry_run > 0
					? `Queued. ${run.policies_in_dry_run} of ${run.policies_enabled} enabled rules are in dry run and will move nothing.`
					: `Queued over ${run.policies_enabled} enabled rule${run.policies_enabled === 1 ? '' : 's'}.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not queue a sweep.';
		}
	}

	// Plain objects and arrays rather than `Map` and `Set`, because the lint rule that asks for `SvelteMap`
	// is right about reactive state and these are neither: both are computed inside a function, read once by
	// the markup, and thrown away. Reaching for the reactive versions would be reactive machinery around a
	// value nothing mutates.

	/** Skips grouped by reason, most numerous first. */
	function grouped(plan: LifecyclePlan): [string, number][] {
		const counts: Record<string, number> = {};
		for (const skip of plan.skipped) {
			counts[skip.reason] = (counts[skip.reason] ?? 0) + 1;
		}
		return Object.entries(counts).sort((a, b) => b[1] - a[1]);
	}

	/** The reasons worth spelling out: a pin is something somebody can go and undo. */
	function pins(plan: LifecyclePlan): string[] {
		const seen: string[] = [];
		for (const skip of plan.skipped) {
			if (skip.reason === 'pinned' && skip.detail && !seen.includes(skip.detail)) {
				seen.push(skip.detail);
			}
		}
		return seen;
	}

	/** What each machine reason is called on screen. */
	const REASONS: Record<string, string> = {
		pinned: 'pinned',
		tier_exempt: 'a thumbnail or proxy, which never tiers',
		not_yet_eligible: 'not idle long enough yet',
		min_duration_not_elapsed: 'still inside the minimum billable period of its current class',
		already_in_class: 'already in the target class',
		would_warm: 'would move to a warmer class, which is a restore rather than a transition',
		below_minimum_size: 'too small to be cheaper in the colder class',
		not_present: 'the copy is not present',
		restore_in_flight: 'a restore is in flight'
	};

	function bytes(n: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}

	function total(plan: LifecyclePlan): number {
		return plan.transitions.reduce((sum, one) => sum + one.size_bytes, 0);
	}
</script>

<svelte:head><title>Storage · damrs</title></svelte:head>

<div class="space-y-6 p-4">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Storage</h1>
		<p class="max-w-2xl text-sm text-muted">
			Tiering rules move originals to colder, cheaper classes once nothing has read them for a
			while. Thumbnails and proxies never move, so search and preview keep working — only the
			original needs a restore, and only when somebody asks for it.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}

	<div class="flex items-center gap-3">
		<button
			type="button"
			class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised"
			onclick={sweep}
		>
			Run a sweep now
		</button>
		{#if queued}
			<p role="status" class="text-xs text-muted">{queued}</p>
		{:else}
			<p class="text-xs text-muted">
				Sweeps run daily on their own. Each rule’s own dry-run setting decides whether it moves
				anything.
			</p>
		{/if}
	</div>

	{#if policies.length === 0}
		<p class="text-sm text-muted">
			No tiering rules are enabled, so nothing is being moved anywhere. Every asset stays in the
			class it was uploaded to.
		</p>
	{/if}

	<ul class="space-y-4">
		{#each policies as policy (policy.id)}
			<li class="space-y-3 rounded-md border border-line p-3">
				<div class="flex flex-wrap items-baseline gap-3">
					<h2 class="text-sm font-semibold tracking-tight">{policy.name}</h2>
					<span class="font-mono text-xs text-muted">→ {policy.target_class}</span>
					<span class="text-xs text-muted">
						after {policy.after_days} idle day{policy.after_days === 1 ? '' : 's'}
					</span>
					{#if policy.dry_run}
						<span class="rounded border border-line px-1.5 py-0.5 text-xs">
							dry run — moves nothing
						</span>
					{/if}
					{#if policy.last_run_at}
						<span class="text-xs text-muted">
							last run <time datetime={policy.last_run_at}
								>{new Date(policy.last_run_at).toLocaleString()}</time
							>, moved {policy.last_run_moved ?? 0}
						</span>
					{:else}
						<span class="text-xs text-muted">never run</span>
					{/if}
					<button
						type="button"
						class="ml-auto rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
						disabled={planning === policy.id}
						onclick={() => plan(policy.id)}
					>
						{planning === policy.id ? 'Planning…' : 'Plan'}
					</button>
				</div>

				{#if plans[policy.id]}
					{@const plan = plans[policy.id]}
					<div class="space-y-2 border-t border-line pt-2 text-xs">
						<p>
							<strong class="tabular-nums">{plan.transitions.length}</strong>
							object{plan.transitions.length === 1 ? '' : 's'} would move
							{#if plan.transitions.length > 0}({bytes(total(plan))}){/if},
							<strong class="tabular-nums">{plan.skipped.length}</strong>
							left alone.
						</p>

						{#if plan.halted}
							<p class="text-state-rights-denied-fg">Stopped early: {plan.halted}</p>
						{/if}

						{#if plan.skipped.length > 0}
							<ul class="space-y-0.5 text-muted">
								{#each grouped(plan) as [reason, count] (reason)}
									<li>
										<span class="tabular-nums">{count}</span>
										— {REASONS[reason] ?? reason}
									</li>
								{/each}
							</ul>
						{/if}

						{#each pins(plan) as detail (detail)}
							<p class="text-muted">Pinned: {detail}</p>
						{/each}

						{#if plan.transitions.length > 0}
							<!-- The first few keys, so an operator can recognise what this is about without a
							     wall of hashes. The count above is the number that matters. -->
							<ul class="space-y-0.5 font-mono text-muted">
								{#each plan.transitions.slice(0, 5) as one (one.object_key)}
									<li class="truncate">
										{one.object_key} · {one.from} → {one.to} · {bytes(one.size_bytes)}
									</li>
								{/each}
							</ul>
							{#if plan.transitions.length > 5}
								<p class="text-muted">…and {plan.transitions.length - 5} more.</p>
							{/if}
						{/if}
					</div>
				{/if}
			</li>
		{/each}
	</ul>
</div>
