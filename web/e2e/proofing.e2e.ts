/**
 * Proofing rounds through a real browser (M6b, §8.1).
 *
 * The Rust suites prove the derived outcome, the snapshot, the reviewer authorisation and the whole-round
 * visibility rule. What lives only here are properties of the *screens*:
 *
 * - **"Waiting on you" is a different list from "rounds you can see"**, and leads. An administrator can see
 *   every round and is a reviewer on almost none.
 * - **Every reviewer is named, answered or not.** "Waiting on Bob" is the value of a review list; a "2 of 3"
 *   summary is the version that makes somebody open a spreadsheet.
 * - **One reviewer asking for changes shows as changes requested** however many others approved — the derived
 *   outcome, visible on the screen rather than only in the database.
 * - **A closed round offers the next round instead of a dead button.** The server's 409 says what to do
 *   instead; a greyed-out control that fails on click says nothing.
 * - **The pictures are the screen**, and an asset with nothing rendered says so rather than showing a broken
 *   image.
 * - **The selection is where a round is asked for**, because the selection is what a round *is*.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Person = { id: string; name: string; email: string };
type Reviewer = {
	person: Person;
	verdict: 'pending' | 'approved' | 'changes_requested';
	note: string;
	decided_at: string | null;
};
type Round = {
	id: string;
	title: string;
	brief: string;
	number: number;
	supersedes: string | null;
	due_at: string | null;
	requested_by: Person | null;
	created_at: string;
	closed_at: string | null;
	outcome: 'open' | 'approved' | 'changes_requested' | 'cancelled';
	asset_count: number;
	reviewers: Reviewer[];
};
type RoundAsset = {
	asset_id: string;
	position: number;
	filename: string;
	mime: string;
	thumbnail_url: string | null;
};

const ADA: Person = { id: 'p-ada', name: 'Ada', email: 'ada@example.com' };
const BOB: Person = { id: 'p-bob', name: 'Bob', email: 'bob@example.com' };
const CARA: Person = { id: 'p-cara', name: 'Cara', email: 'cara@example.com' };

function reviewer(person: Person, overrides: Partial<Reviewer> = {}): Reviewer {
	return { person, verdict: 'pending', note: '', decided_at: null, ...overrides };
}

function round(overrides: Partial<Round> = {}): Round {
	return {
		id: 'aaaaaaaa-1111-4111-8111-111111111111',
		title: 'Spring campaign crops',
		brief: 'Check the crops against the layout.',
		number: 1,
		supersedes: null,
		due_at: null,
		requested_by: ADA,
		created_at: '2026-08-20T09:00:00Z',
		closed_at: null,
		outcome: 'open',
		asset_count: 2,
		reviewers: [reviewer(BOB), reviewer(CARA)],
		...overrides
	};
}

function asset(name: string, overrides: Partial<RoundAsset> = {}): RoundAsset {
	return {
		asset_id: `${name}0000-1111-4111-8111-111111111111`.slice(0, 36),
		position: 0,
		filename: `${name}.jpg`,
		mime: 'image/jpeg',
		thumbnail_url: null,
		...overrides
	};
}

type Options = {
	rounds?: Round[];
	mine?: Round[];
	assets?: RoundAsset[];
	/** What the server answers a verdict with — the derived outcome, which the screen must not guess. */
	afterVerdict?: Round;
	/** A refusal to exercise, keyed by the path suffix the call ends with. */
	refuse?: { suffix: string; status: number; body?: unknown };
};

async function connect(page: Page, options: Options = {}) {
	const recorder = {
		verdicts: [] as { id: string; verdict: string; note: string }[],
		opened: [] as { title: string; asset_ids: string[]; reviewer_ids: string[] }[],
		cancelled: [] as string[]
	};

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		const path = url.pathname;

		if (options.refuse && path.endsWith(options.refuse.suffix)) {
			return route.fulfill({
				status: options.refuse.status,
				json: options.refuse.body ?? { reason: 'refused' }
			});
		}

		if (path === '/proofing' && method === 'GET') {
			return route.fulfill({ json: options.rounds ?? [round()] });
		}
		if (path === '/proofing/mine' && method === 'GET') {
			return route.fulfill({ json: options.mine ?? [] });
		}
		if (path === '/proofing' && method === 'POST') {
			const body = route.request().postDataJSON() as {
				title: string;
				asset_ids: string[];
				reviewer_ids: string[];
			};
			recorder.opened.push(body);
			return route.fulfill({
				status: 201,
				json: round({
					title: body.title,
					asset_count: body.asset_ids.length,
					reviewers: body.reviewer_ids.map((id) =>
						reviewer([ADA, BOB, CARA].find((one) => one.id === id) ?? BOB)
					)
				})
			});
		}
		if (path.endsWith('/verdict') && method === 'POST') {
			const body = route.request().postDataJSON() as { verdict: string; note: string };
			recorder.verdicts.push({ id: path.split('/')[2] ?? '', ...body });
			return route.fulfill({ json: options.afterVerdict ?? round({ outcome: 'open' }) });
		}
		if (path.endsWith('/cancel') && method === 'POST') {
			recorder.cancelled.push(path.split('/')[2] ?? '');
			return route.fulfill({
				json: round({ outcome: 'cancelled', closed_at: '2026-08-23T12:00:00Z' })
			});
		}
		// Prefixed as well as suffixed: `'/assets'.endsWith('/assets')` is true, so a bare suffix match here
		// swallowed the grid's own listing and left every selection test with an empty page.
		if (path.startsWith('/proofing/') && path.endsWith('/assets') && method === 'GET') {
			return route.fulfill({ json: options.assets ?? [] });
		}
		if (path.startsWith('/proofing/') && method === 'GET') {
			return route.fulfill({ json: (options.rounds ?? [round()])[0] });
		}
		// Enough of the grid for a selection to exist, which is the only way to ask for a round. Two assets,
		// because a round over one picture would not prove the selection travels.
		if (path === '/assets') {
			return route.fulfill({
				json: {
					items: [0, 1].map((index) => ({
						id: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`,
						filename: `campaign-${index}.jpg`,
						mime: 'image/jpeg',
						bytes: 2_400_000,
						width: 4000,
						height: 3000,
						tier: 'hot',
						rights_state: 'allowed',
						provenance_state: 'none',
						thumbnail_url: null,
						tag_confidence: null,
						is_favourite: false,
						average_stars: null,
						has_attachment: false,
						published_at: null
					})),
					total: 2,
					offset: 0
				}
			});
		}
		// An array, which is what the page expects. An object here made the rail throw mid-render and left the
		// grid stuck on "Searching…" with no error anywhere — the shape has to be right even when it is empty.
		if (path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') {
			return route.fulfill({ json: [ADA, BOB, CARA] });
		}
		if (path === '/me') {
			return route.fulfill({ json: ADA });
		}
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('what is waiting on you is a separate list, and leads', async ({ page }) => {
	// The whole reason the two calls exist. An administrator sees every round and is a reviewer on almost
	// none, so merging them would bury the one thing somebody is blocking on.
	await connect(page, {
		rounds: [
			round({ id: 'a1111111-1111-4111-8111-111111111111', title: 'Everything I can see' }),
			round({ id: 'b1111111-1111-4111-8111-111111111111', title: 'Also visible to me' })
		],
		mine: [round({ id: 'a1111111-1111-4111-8111-111111111111', title: 'Everything I can see' })]
	});
	await page.goto('/proofing');

	await expect(page.getByRole('heading', { name: 'Waiting on you' })).toBeVisible();
	await expect(page.getByTestId('mine').getByRole('link')).toHaveCount(1);
	await expect(page.getByTestId('all').getByRole('link')).toHaveCount(2);
	await expect(page.getByText('2 open of 2')).toBeVisible();
});

test('nothing waiting on you says so, and says the rest are below', async ({ page }) => {
	// An empty list here is the good outcome and must not read like a failure to load.
	await connect(page, { mine: [] });
	await page.goto('/proofing');
	await expect(page.getByTestId('none-waiting')).toContainText(
		'Nothing is waiting on your verdict'
	);
	await expect(page.getByTestId('none-waiting')).toContainText('not asked about are below');
});

test('every reviewer is named, including the ones who have not answered', async ({ page }) => {
	await connect(page, {
		rounds: [
			round({
				reviewers: [
					reviewer(BOB, {
						verdict: 'approved',
						note: 'looks right',
						decided_at: '2026-08-21T10:00:00Z'
					}),
					reviewer(CARA)
				]
			})
		]
	});
	await page.goto('/proofing');

	// Both names, and the standing of each. Not "1 of 2".
	await expect(page.getByTestId('all').getByText('Bob')).toBeVisible();
	await expect(page.getByTestId('all').getByText('Cara')).toBeVisible();
	await expect(page.getByTestId('all').getByText('waiting')).toBeVisible();
	await expect(page.getByTestId('all').getByText('approved')).toBeVisible();
});

test('one reviewer asking for changes outranks any number of approvals', async ({ page }) => {
	// The derived outcome, on the screen. `changes_requested` wins, and the page says why so nobody reads it
	// as a bug — two people approved and it is not approved.
	await connect(page, {
		rounds: [
			round({
				outcome: 'changes_requested',
				closed_at: '2026-08-21T11:00:00Z',
				reviewers: [
					reviewer(ADA, { verdict: 'approved', decided_at: '2026-08-21T10:00:00Z' }),
					reviewer(BOB, { verdict: 'approved', decided_at: '2026-08-21T10:30:00Z' }),
					reviewer(CARA, {
						verdict: 'changes_requested',
						note: 'tighter crops',
						decided_at: '2026-08-21T11:00:00Z'
					})
				]
			})
		]
	});
	await page.goto('/proofing');

	await expect(page.getByTestId('outcome-aaaaaaaa-1111-4111-8111-111111111111')).toHaveText(
		'changes requested'
	);
	await expect(page.getByText('however many others approved', { exact: false })).toBeVisible();
});

test('a round shows its pictures, and says when one has none rendered', async ({ page }) => {
	await connect(page, {
		assets: [
			asset('harbour', { position: 0, thumbnail_url: '/d/abc' }),
			asset('quayside', { position: 1 })
		]
	});
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');

	await expect(page.getByRole('heading', { name: '2 assets', exact: false })).toBeVisible();
	await expect(page.getByTestId('round-assets').locator('img')).toHaveCount(1);
	// Absent is not an error, and the screen says which it is rather than showing a broken image.
	await expect(page.getByText('no preview yet')).toBeVisible();
	await expect(page.getByText('harbour.jpg')).toBeVisible();
	await expect(page.getByText('quayside.jpg')).toBeVisible();
});

test('a verdict takes its outcome from the server, not from the button pressed', async ({
	page
}) => {
	// Approving does not make a round approved: somebody else may have asked for changes. The reply carries
	// the derived outcome and the screen reports that.
	const recorder = await connect(page, {
		assets: [asset('harbour')],
		afterVerdict: round({
			outcome: 'open',
			reviewers: [reviewer(BOB, { verdict: 'approved' }), reviewer(CARA)]
		})
	});
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');

	await page.getByTestId('note').fill('fine by me');
	await page.getByTestId('approve').click();

	// The live region, which is the copy that matters: a sighted reader sees the same sentence beside the
	// buttons, and asserting on the announced one covers both.
	await expect(page.getByRole('status')).toContainText('stays open for the others');
	expect(recorder.verdicts).toEqual([
		{
			id: 'aaaaaaaa-1111-4111-8111-111111111111',
			verdict: 'approved',
			note: 'fine by me'
		}
	]);
});

test('a closed round offers the next round instead of a button that fails', async ({ page }) => {
	await connect(page, {
		rounds: [round({ outcome: 'approved', closed_at: '2026-08-21T11:00:00Z' })],
		assets: [asset('harbour')]
	});
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');

	// No verdict controls at all, rather than disabled ones that 409 on click.
	await expect(page.getByTestId('approve')).toHaveCount(0);
	await expect(page.getByTestId('request-changes')).toHaveCount(0);
	await expect(page.getByText('a new round over the same assets', { exact: false })).toBeVisible();
	// And withdrawing a closed round is not offered either: there is nothing left to withdraw.
	await expect(page.getByTestId('withdraw')).toHaveCount(0);
});

test('withdrawing is confirmed inline and keeps the verdicts already given', async ({ page }) => {
	const recorder = await connect(page, { assets: [asset('harbour')] });
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');

	await page.getByTestId('withdraw').click();
	// Inline rather than a confirm(), which blocks the automation driving this page.
	await expect(page.getByText('verdicts already given are kept', { exact: false })).toBeVisible();
	await page.getByTestId('withdraw-confirm').click();

	await expect(page.getByTestId('outcome')).toHaveText('cancelled');
	expect(recorder.cancelled).toEqual(['aaaaaaaa-1111-4111-8111-111111111111']);
});

test('a round you cannot fully see reads as one refusal, not as a load failure', async ({
	page
}) => {
	// The server gives the same 404 for "no such round" and "not all of its assets are visible", deliberately —
	// distinguishing them would confirm the round exists. The screen has to say both without implying a bug.
	await connect(page, { refuse: { suffix: '111111111111', status: 404 } });
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');

	await expect(page.getByRole('alert')).toContainText('No such round');
	await expect(page.getByRole('alert')).toContainText('assets you cannot see');
});

test('a verdict from somebody not on the list explains itself', async ({ page }) => {
	await connect(page, {
		assets: [asset('harbour')],
		refuse: { suffix: '/verdict', status: 403 }
	});
	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');
	await page.getByTestId('approve').click();
	await expect(page.getByRole('alert')).toContainText('not a reviewer on this round');
});

test('the selection is where a round is asked for', async ({ page }) => {
	// A round is a fixed set of assets, and the grid selection is that set. Asking from anywhere else would
	// mean a second way to choose assets.
	const recorder = await connect(page);
	await page.goto('/assets');

	const cells = page.getByRole('gridcell');
	await expect(cells.first()).toBeVisible();
	await cells.nth(0).click();
	await cells.nth(1).click({ modifiers: ['Shift'] });

	await page.getByTestId('review-open').click();
	await page.getByTestId('review-title').fill('Spring campaign crops');
	// Two colleagues can share a display name, so the picker shows the email too.
	await expect(page.getByText('bob@example.com')).toBeVisible();
	await page.getByTestId(`reviewer-${BOB.id}`).check();
	await page.getByTestId('review-send').click();

	await expect(page.getByTestId('reviewed')).toContainText('Sent to 1 for review');
	expect(recorder.opened).toHaveLength(1);
	expect(recorder.opened[0].title).toBe('Spring campaign crops');
	expect(recorder.opened[0].reviewer_ids).toEqual([BOB.id]);
	expect(recorder.opened[0].asset_ids.length).toBeGreaterThan(1);
});

test('sending needs both a title and somebody to ask', async ({ page }) => {
	// A round with no title is a line in somebody's list saying nothing; a round with no reviewers asks nobody
	// anything. Both are refused before the request rather than by a 422 afterwards.
	await connect(page);
	await page.goto('/assets');
	const cells = page.getByRole('gridcell');
	await expect(cells.first()).toBeVisible();
	await cells.nth(0).click();

	await page.getByTestId('review-open').click();
	await expect(page.getByTestId('review-send')).toBeDisabled();
	await page.getByTestId('review-title').fill('Just a title');
	await expect(page.getByTestId('review-send')).toBeDisabled();
	await page.getByTestId(`reviewer-${BOB.id}`).check();
	await expect(page.getByTestId('review-send')).toBeEnabled();
});

test('the proofing screens have no axe violations', async ({ page }) => {
	await connect(page, {
		rounds: [
			round({
				reviewers: [
					reviewer(BOB, { verdict: 'approved', decided_at: '2026-08-21T10:00:00Z' }),
					reviewer(CARA)
				]
			}),
			round({ id: 'b1111111-1111-4111-8111-111111111111', outcome: 'cancelled', number: 2 })
		],
		mine: [round()],
		assets: [asset('harbour', { thumbnail_url: '/d/abc' }), asset('quayside', { position: 1 })]
	});

	await page.goto('/proofing');
	await expect(page.getByTestId('all').getByRole('link')).toHaveCount(2);
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);

	await page.goto('/proofing/aaaaaaaa-1111-4111-8111-111111111111');
	await expect(page.getByTestId('round-assets')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
