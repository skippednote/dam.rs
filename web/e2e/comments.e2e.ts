/**
 * The comment panel through a real browser, against a mocked API.
 *
 * The Rust gate proves the two gates, the threading and the refusals. What only exists here is the browser half,
 * and four properties that are decisions about the *interface*:
 *
 * - **The compose box states the consequence, not the setting.** "Only you and the people you choose" rather than a
 *   switch labelled private. Somebody who infers wrongly cannot take the words back.
 * - **A private comment with nobody named cannot be sent.** The server refuses it; a form that lets you write three
 *   paragraphs first wasted your time on purpose.
 * - **Edit and Delete appear only on your own comments.** An affordance that exists only to be refused teaches
 *   people to distrust every control beside it.
 * - **No reply control on a reply.** Threads are one level deep and the server refuses a deeper one.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

const ASSET = '00000000-0000-4000-8000-000000000000';
/** A second asset, so switching selection can be exercised at all. */
const OTHER = '00000000-0000-4000-8000-000000000001';
const ADA = '00000000-0000-4000-9000-00000000000a';
const GRACE = '00000000-0000-4000-9000-00000000000b';

type Comment = {
	id: string;
	asset_id: string;
	author: { id: string; name: string; email: string };
	body: string;
	visibility: 'public' | 'private';
	status: 'open' | 'resolved' | 'approved' | 'changes_requested';
	status_by: { id: string; name: string; email: string } | null;
	parent_id: string | null;
	recipients: { id: string; name: string; email: string }[];
	created_at: string;
	edited_at: string | null;
};

const PEOPLE = [
	{ id: ADA, name: 'Ada Lovelace', email: 'ada@example.com' },
	{ id: GRACE, name: 'Grace Hopper', email: 'grace@example.com' }
];

function comment(overrides: Partial<Comment> = {}): Comment {
	return {
		id: `c-${Math.abs(hash(JSON.stringify(overrides)))}`,
		asset_id: ASSET,
		author: PEOPLE[0],
		body: 'The crop is tight',
		visibility: 'public',
		status: 'open',
		status_by: null,
		parent_id: null,
		recipients: [],
		created_at: '2026-08-01T09:00:00Z',
		edited_at: null,
		...overrides
	};
}

/** A stable id from the overrides, so a fixture's ids do not change between runs. */
function hash(text: string): number {
	let value = 0;
	for (const character of text) value = (value * 31 + character.codePointAt(0)!) | 0;
	return value;
}

type Recorder = {
	posts: Record<string, unknown>[];
	patches: { id: string; body: Record<string, unknown> }[];
	deletes: string[];
};

async function connect(
	page: Page,
	options: {
		comments?: Comment[];
		/** Lets a case make one of the three reads fail. */
		commentsStatus?: number;
		peopleStatus?: number;
		meStatus?: number;
		/** Only the caller is in the tenant, so there is nobody to address a private comment to. */
		alone?: boolean;
		refusePost?: { status: number; reason: string };
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = { posts: [], patches: [], deletes: [] };
	let comments = options.comments ?? [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		const path = url.pathname;

		if (path === '/me') {
			if (options.meStatus) return route.fulfill({ status: options.meStatus, json: {} });
			return route.fulfill({ json: PEOPLE[0] });
		}
		if (path === '/people') {
			if (options.peopleStatus) return route.fulfill({ status: options.peopleStatus, json: [] });
			// `alone` models a tenant with one member, which is where the private option becomes a dead end.
			return route.fulfill({ json: options.alone ? [PEOPLE[0]] : PEOPLE });
		}
		if (path === `/assets/${OTHER}/comments` && method === 'GET') {
			// The second asset's conversation is empty, so a draft carried across selections would be visible.
			return route.fulfill({ json: [] });
		}
		if (path.endsWith('/comments') && method === 'GET') {
			if (options.commentsStatus) {
				return route.fulfill({
					status: options.commentsStatus,
					json: { reason: 'the comment table is not there' }
				});
			}
			return route.fulfill({ json: comments });
		}
		if (path.endsWith('/comments') && method === 'POST') {
			const posted = route.request().postDataJSON() as Record<string, unknown>;
			recorder.posts.push(posted);
			if (options.refusePost) {
				return route.fulfill({
					status: options.refusePost.status,
					json: { reason: options.refusePost.reason }
				});
			}
			const made = comment({
				id: `c-new-${comments.length}`,
				body: String(posted.body),
				visibility: (posted.visibility as Comment['visibility']) ?? 'public',
				parent_id: (posted.parent_id as string | null) ?? null,
				recipients: ((posted.recipients as string[]) ?? []).map(
					(id) => PEOPLE.find((person) => person.id === id) ?? PEOPLE[1]
				)
			});
			comments = [...comments, made];
			return route.fulfill({ status: 201, json: made });
		}
		if (path.startsWith('/comments/') && method === 'PATCH') {
			const id = path.split('/').pop() ?? '';
			const change = route.request().postDataJSON() as Record<string, unknown>;
			recorder.patches.push({ id, body: change });
			comments = comments.map((row) =>
				row.id === id
					? {
							...row,
							...(typeof change.body === 'string'
								? { body: change.body, edited_at: '2026-08-02T09:00:00Z' }
								: {}),
							...(typeof change.status === 'string'
								? { status: change.status as Comment['status'], status_by: PEOPLE[0] }
								: {})
						}
					: row
			);
			return route.fulfill({ json: comments.find((row) => row.id === id) });
		}
		if (path.startsWith('/comments/') && method === 'DELETE') {
			const id = path.split('/').pop() ?? '';
			recorder.deletes.push(id);
			// Replies go with the parent, as the server's cascade does.
			comments = comments.filter((row) => row.id !== id && row.parent_id !== id);
			return route.fulfill({ status: 204, body: '' });
		}

		if (path === '/assets') {
			return route.fulfill({ json: { items: [summary(), summary(OTHER)], total: 2, offset: 0 } });
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/search') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
		}
		if (path.startsWith('/assets/') && path.endsWith('/categories')) {
			return route.fulfill({ json: [] });
		}
		if (path.startsWith('/assets/') && path.endsWith('/type')) {
			return route.fulfill({ json: { field_keys: [] } });
		}
		if (path.startsWith('/assets/')) {
			const id = path.split('/')[2] ?? ASSET;
			return route.fulfill({
				json: {
					...summary(id),
					values: {},
					technical: {},
					duration_ms: null,
					page_count: null,
					color_space: 'sRGB',
					has_alpha: false,
					content_hash: 'a'.repeat(64),
					status: 'active',
					enrichment_state: 'done',
					legal_hold: false,
					release_at: null,
					expires_at: null,
					version_no: 1,
					created_at: '2026-08-01T09:00:00Z',
					updated_at: '2026-08-01T09:00:00Z',
					engagement: {
						asset_id: ASSET,
						average_stars: null,
						rating_count: 0,
						favourite_count: 0,
						my_stars: null,
						is_favourite: false,
						is_watched: false
					},
					preview_url: null
				}
			});
		}
		return route.fulfill({ status: 404, json: {} });
	});

	function summary(id: string = ASSET) {
		return {
			id,
			filename: id === OTHER ? 'quay.jpg' : 'harbour.jpg',
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
			// Paperwork flag, as every summary now carries it (Q.9).
			has_attachment: false
		};
	}

	return recorder;
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Comments' });
}

/** Opens the asset so the detail panel — and the comment panel in it — is on screen. */
async function open(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').first().dblclick();
	await expect(panel(page)).toBeVisible();
}

test('an empty thread says so, and the box defaults to everyone', async ({ page }) => {
	await connect(page);
	await open(page);

	await expect(panel(page)).toContainText('No comments yet');
	// Public is the default because private is the deliberate choice — and the option says what it *does*.
	await expect(
		panel(page).getByRole('radio', { name: 'Everyone who can see this asset' })
	).toBeChecked();
	await expect(
		panel(page).getByRole('radio', { name: 'Only you and the people you choose' })
	).not.toBeChecked();
	await expect(panel(page).getByRole('button', { name: 'Post' })).toBeDisabled();
});

test('a private comment cannot be sent until somebody is named', async ({ page }) => {
	const recorder = await connect(page);
	await open(page);

	await panel(page)
		.getByRole('textbox', { name: 'Add a comment' })
		.fill('Legal has not cleared this');
	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();

	// Said before sending rather than refused after: the server rejects a private comment addressed to nobody,
	// and a form that takes three paragraphs first wasted your time on purpose.
	await expect(panel(page)).toContainText('nobody but you will ever see this');
	await expect(panel(page).getByRole('button', { name: 'Post privately' })).toBeDisabled();

	await panel(page).getByRole('listbox', { name: 'Who can see it' }).selectOption([GRACE]);
	// Named, so the person writing it can check the audience before hitting send.
	await expect(panel(page)).toContainText('Only Grace Hopper will see this');

	const send = panel(page).getByRole('button', { name: 'Post privately' });
	await expect(send).toBeEnabled();
	await send.click();

	await expect(panel(page).getByText('Legal has not cleared this')).toBeVisible();
	expect(recorder.posts).toEqual([
		{
			body: 'Legal has not cleared this',
			visibility: 'private',
			recipients: [GRACE]
		}
	]);
});

test('a public comment sends no recipients at all', async ({ page }) => {
	const recorder = await connect(page);
	await open(page);

	await panel(page).getByRole('textbox', { name: 'Add a comment' }).fill('Looks good');
	await panel(page).getByRole('button', { name: 'Post', exact: true }).click();

	await expect(panel(page).getByText('Looks good')).toBeVisible();
	// Not the recipient list from a previous private draft: a public comment routed to somebody would notify them
	// about something they could already see, which is noise rather than routing.
	expect(recorder.posts).toEqual([{ body: 'Looks good', visibility: 'public', recipients: [] }]);
});

test('changing your mind back to public drops the recipients', async ({ page }) => {
	// The stale-state case: name somebody while private, then change the visibility. A public comment routed to
	// people would notify them about something they can already see, and it means the form kept a decision the
	// person visibly reversed.
	const recorder = await connect(page);
	await open(page);

	await panel(page)
		.getByRole('textbox', { name: 'Add a comment' })
		.fill('Actually this is fine to share');
	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();
	await panel(page).getByRole('listbox', { name: 'Who can see it' }).selectOption([GRACE]);
	await expect(panel(page)).toContainText('Only Grace Hopper will see this');

	await panel(page).getByRole('radio', { name: 'Everyone who can see this asset' }).check();
	await panel(page).getByRole('button', { name: 'Post', exact: true }).click();

	await expect(panel(page).getByText('Actually this is fine to share')).toBeVisible();
	expect(recorder.posts).toEqual([
		{ body: 'Actually this is fine to share', visibility: 'public', recipients: [] }
	]);
});

test('a private comment shows who it reaches, and says private', async ({ page }) => {
	await connect(page, {
		comments: [
			comment({
				id: 'c-private',
				body: 'Do not release before Friday',
				visibility: 'private',
				recipients: [PEOPLE[1]]
			})
		]
	});
	await open(page);

	await expect(panel(page)).toContainText('private');
	// Spelled out on the comment itself, not only in the compose box: somebody reading a thread later needs to
	// know who saw this without reconstructing it.
	await expect(panel(page)).toContainText('Visible to Ada Lovelace and Grace Hopper');
});

test('edit and delete appear only on your own comments', async ({ page }) => {
	await connect(page, {
		comments: [
			comment({ id: 'c-mine', body: 'Mine', author: PEOPLE[0] }),
			comment({ id: 'c-theirs', body: 'Theirs', author: PEOPLE[1] })
		]
	});
	await open(page);

	// One of each control, on the caller's own comment. Offering them on somebody else's would put a control on
	// screen that exists only to be refused.
	await expect(panel(page).getByRole('button', { name: 'Edit' })).toHaveCount(1);
	await expect(panel(page).getByRole('button', { name: 'Delete' })).toHaveCount(1);

	// The status control is on both, because a status is any reader's to move — `approved` is somebody else's
	// verdict, so a control only the author could use could never mean approval.
	await expect(panel(page).getByRole('combobox')).toHaveCount(2);
});

test('editing sends only the body and reports that it is marked', async ({ page }) => {
	const recorder = await connect(page, {
		comments: [comment({ id: 'c-mine', body: 'Lower the highlights' })]
	});
	await open(page);

	await panel(page).getByRole('button', { name: 'Edit' }).click();
	await panel(page).getByRole('textbox', { name: 'Edit comment' }).fill('Lower them a little');
	await panel(page).getByRole('button', { name: 'Save' }).click();

	await expect(panel(page).getByText('Lower them a little')).toBeVisible();
	// The badge on the comment, not the panel's text: the success notice also says "marked as edited", so an
	// assertion against the whole panel passed whether or not the badge rendered. Mutation testing said so.
	await expect(panel(page).getByText('· edited')).toBeVisible();
	// Only the body — never a body and a status together, which the server refuses outright.
	expect(recorder.patches).toEqual([{ id: 'c-mine', body: { body: 'Lower them a little' } }]);
});

test('moving a status sends only the status and names who moved it', async ({ page }) => {
	const recorder = await connect(page, {
		comments: [comment({ id: 'c-theirs', author: PEOPLE[1], body: 'Needs a wider crop' })]
	});
	await open(page);

	await panel(page).getByRole('combobox').selectOption('approved');

	await expect(panel(page)).toContainText('Approved');
	await expect(panel(page)).toContainText('by Ada Lovelace');
	expect(recorder.patches).toEqual([{ id: 'c-theirs', body: { status: 'approved' } }]);
});

test('a reply threads under its parent, and a reply has no reply control', async ({ page }) => {
	const recorder = await connect(page, {
		comments: [comment({ id: 'c-parent', body: 'Is this final?' })]
	});
	await open(page);

	await expect(panel(page).getByRole('button', { name: 'Reply' })).toHaveCount(1);
	await panel(page).getByRole('button', { name: 'Reply' }).click();
	await panel(page)
		.getByRole('textbox', { name: /Reply to/ })
		.fill('Yes, signed off');
	await panel(page).getByRole('button', { name: 'Post reply' }).click();

	await expect(panel(page).getByText('Yes, signed off')).toBeVisible();
	expect(recorder.posts).toEqual([{ body: 'Yes, signed off', parent_id: 'c-parent' }]);

	// Still one Reply control: threads are one level deep and the server refuses a deeper one, so there is no
	// affordance on the reply itself.
	await expect(panel(page).getByRole('button', { name: 'Reply' })).toHaveCount(1);
});

test('deleting a comment with replies says it takes them too', async ({ page }) => {
	const recorder = await connect(page, {
		comments: [
			comment({ id: 'c-parent', body: 'Which version shipped?' }),
			comment({ id: 'c-reply', body: 'The second', parent_id: 'c-parent', author: PEOPLE[1] })
		]
	});
	await open(page);

	await panel(page).getByRole('button', { name: 'Delete' }).click();
	// The consequence, counted: a reply to a question that no longer exists reads as corruption.
	await expect(panel(page)).toContainText('also deletes the 1 reply to it');

	await panel(page).getByRole('button', { name: 'Delete', exact: true }).last().click();
	await expect(panel(page).getByText('Which version shipped?')).toHaveCount(0);
	await expect(panel(page).getByText('The second')).toHaveCount(0);
	expect(recorder.deletes).toEqual(['c-parent']);
});

test("a refusal is shown in the server's own words", async ({ page }) => {
	await connect(page, {
		refusePost: { status: 422, reason: 'a comment needs between 1 and 10000 characters' }
	});
	await open(page);

	await panel(page).getByRole('textbox', { name: 'Add a comment' }).fill('x');
	await panel(page).getByRole('button', { name: 'Post', exact: true }).click();
	await expect(panel(page).getByRole('alert')).toContainText('between 1 and 10000 characters');
});

test('a failing roster does not empty the thread', async ({ page }) => {
	// Three independent reads. The auto-import picker lost its options to an unrelated 500 once; this is the same
	// shape, so the reads settle separately.
	await connect(page, {
		comments: [comment({ id: 'c-one', body: 'Still readable' })],
		peopleStatus: 500,
		meStatus: 500
	});
	await open(page);

	await expect(panel(page).getByText('Still readable')).toBeVisible();
	// The Edit control is the only casualty of not knowing who the reader is — and it is the honest casualty,
	// because offering it without knowing would be offering something the server may refuse.
	await expect(panel(page).getByRole('button', { name: 'Edit' })).toHaveCount(0);
});

test('a failing thread read is reported and does not break the panel', async ({ page }) => {
	await connect(page, { commentsStatus: 500 });
	await open(page);

	await expect(panel(page).getByRole('alert')).toContainText('comment table is not there');
	// The compose box still stands: being unable to read the conversation does not mean being unable to add to it,
	// and hiding the form would make a transient failure look like a missing feature.
	await expect(panel(page).getByRole('textbox', { name: 'Add a comment' })).toBeVisible();
});

test('the panel has no accessibility violations', async ({ page }) => {
	await connect(page, {
		comments: [
			comment({
				id: 'c-parent',
				body: 'Is this final?',
				status: 'changes_requested',
				status_by: PEOPLE[1]
			}),
			comment({ id: 'c-reply', body: 'Yes', parent_id: 'c-parent', author: PEOPLE[1] }),
			comment({
				id: 'c-private',
				body: 'Between us',
				visibility: 'private',
				recipients: [PEOPLE[1]]
			})
		]
	});
	await open(page);

	const populated = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Comments"]')
		.analyze();
	expect(populated.violations).toEqual([]);

	// And with the private compose branch open, which adds a multi-select and a warning.
	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();
	const composing = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Comments"]')
		.analyze();
	expect(composing.violations).toEqual([]);
});

test('selecting another asset clears the draft rather than carrying it over', async ({ page }) => {
	// A draft that followed the selection would put words written about one asset into a comment on another — and
	// the private audience with them. There is no symptom until it is sent.
	const recorder = await connect(page, {
		comments: [comment({ id: 'c-one', body: 'On the harbour' })]
	});
	await open(page);

	await panel(page)
		.getByRole('textbox', { name: 'Add a comment' })
		.fill('Half-written thought about the harbour');
	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();
	await panel(page).getByRole('listbox', { name: 'Who can see it' }).selectOption([GRACE]);

	await page.getByRole('gridcell').nth(1).dblclick();
	await expect(panel(page)).toContainText('No comments yet');

	// Empty box, public again, and no picker: the audience resets with the words.
	await expect(panel(page).getByRole('textbox', { name: 'Add a comment' })).toHaveValue('');
	await expect(
		panel(page).getByRole('radio', { name: 'Everyone who can see this asset' })
	).toBeChecked();
	await expect(panel(page).getByRole('listbox', { name: 'Who can see it' })).toHaveCount(0);
	expect(recorder.posts).toEqual([]);
});

test('with nobody else here, the private option says so instead of showing an empty picker', async ({
	page
}) => {
	// Found by driving a real dev tenant with one identity. The option was offered, the picker was empty, and the
	// send control stayed disabled with nothing explaining why — a dead end rather than a refusal.
	await connect(page, { alone: true });
	await open(page);

	await panel(page).getByRole('textbox', { name: 'Add a comment' }).fill('Only for me, apparently');
	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();

	await expect(panel(page)).toContainText('There is nobody else here yet');
	// No picker at all, rather than an empty one: an empty listbox beside "choose at least one person" is a
	// requirement that cannot be met and does not say so.
	await expect(panel(page).getByRole('listbox', { name: 'Who can see it' })).toHaveCount(0);
	await expect(panel(page).getByText('nobody but you will ever see this')).toHaveCount(0);
	// And the way out is named: post it publicly instead.
	await expect(panel(page)).toContainText('everyone who can see this asset will');
});

test('the picker never offers the caller themselves', async ({ page }) => {
	// Addressing a private comment to yourself is the one choice that cannot mean anything: you are already its
	// author and already its audience.
	await connect(page);
	await open(page);

	await panel(page).getByRole('radio', { name: 'Only you and the people you choose' }).check();
	const offered = await panel(page)
		.getByRole('listbox', { name: 'Who can see it' })
		.locator('option')
		.allTextContents();
	expect(offered).toEqual(['Grace Hopper · grace@example.com']);
});
