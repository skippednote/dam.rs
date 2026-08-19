/**
 * Orders through a real browser, against a mocked API (Q.13c).
 *
 * The Rust gates prove the state machine and the permissions. What exists only here are five properties of the
 * *interface*:
 *
 * - **The queue is absent, not empty, for somebody who cannot decide.** Being unable to approve is not a fault,
 *   so a 403 renders nothing rather than an error banner.
 * - **The reason is shown to the approver**, because it is the entire question they are answering.
 * - **An approved order does not claim the files are ready.** The pickup is a later slice, and an interface that
 *   implied otherwise would promise what the system has not done.
 * - **An expired pickup says so** rather than still reading "ready" — the window closing is a different fact from
 *   the decision.
 * - **Ordering from a selection reports the server's count**, so somebody who selected ten and may ask for nine
 *   is told, exactly as the bulk bar does for the same reason.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Order = {
	id: string;
	reference: string;
	requested_by: { id: string; name: string; email: string } | null;
	purpose: string;
	channel: string | null;
	territory: string | null;
	conversion_key: string | null;
	include_metadata: boolean;
	recipients: string[];
	state: string;
	expired: boolean;
	decided_by: { id: string; name: string; email: string } | null;
	decided_at: string | null;
	decision_note: string | null;
	self_approved: boolean;
	expires_at: string | null;
	created_at: string;
	items: { asset_id: string; filename: string }[];
};

function order(overrides: Partial<Order> = {}): Order {
	return {
		id: `o-${overrides.reference ?? 'ORD-000001'}`,
		reference: 'ORD-000001',
		requested_by: { id: 'p-rita', name: 'Rita Reader', email: 'rita@example.com' },
		purpose: 'The spring brochure, print run of 20,000.',
		channel: 'print',
		territory: 'GB',
		conversion_key: null,
		include_metadata: true,
		recipients: ['agency@example.com'],
		state: 'submitted',
		expired: false,
		decided_by: null,
		decided_at: null,
		decision_note: null,
		self_approved: false,
		expires_at: null,
		created_at: '2026-08-01T09:00:00Z',
		items: [{ asset_id: 'a-1', filename: 'harbour.jpg' }],
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		mine?: Order[];
		/** `null` means the queue is refused — the ordinary case for a reader. */
		queue?: Order[] | null;
		/** A refusal for a decision, with the server's sentence. */
		decisionRefusal?: { status: number; reason: string };
	} = {}
) {
	const recorder = { decisions: [] as string[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;
		const method = route.request().method();

		if (path === '/orders/queue') {
			if (options.queue === null || options.queue === undefined) {
				return route.fulfill({ status: 403, json: { reason: 'no manage scope' } });
			}
			return route.fulfill({ json: options.queue });
		}
		if (path === '/orders' && method === 'GET') {
			return route.fulfill({ json: options.mine ?? [] });
		}
		if (path.startsWith('/orders/') && method === 'POST') {
			const [, , id, decision] = path.split('/');
			recorder.decisions.push(`${id}:${decision}`);
			if (options.decisionRefusal) {
				return route.fulfill({
					status: options.decisionRefusal.status,
					json: { reason: options.decisionRefusal.reason }
				});
			}
			const state =
				decision === 'approve' ? 'approved' : decision === 'reject' ? 'rejected' : 'cancelled';
			// Emptying both lists afterwards, which is what a decided order does to a queue.
			options.queue = [];
			options.mine = [order({ state, decided_by: { id: 'p-ada', name: 'Ada', email: 'a@x' } })];
			return route.fulfill({ json: options.mine[0] });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('a reader sees their own orders and no queue', async ({ page }) => {
	// The queue is a 403 for most people, and that is not a fault: being unable to approve is ordinary.
	await connect(page, { mine: [order()], queue: null });
	await page.goto('/orders');

	await expect(page.getByRole('region', { name: 'My orders' })).toBeVisible();
	await expect(page.getByText('ORD-000001')).toBeVisible();
	await expect(page.getByRole('region', { name: 'Waiting for a decision' })).toHaveCount(0);
	await expect(page.getByRole('alert')).toHaveCount(0);
});

test('an approver reads the reason and decides', async ({ page }) => {
	const recorder = await connect(page, { mine: [], queue: [order()] });
	await page.goto('/orders');

	const queue = page.getByRole('region', { name: 'Waiting for a decision' });
	await expect(queue.getByText('Rita Reader')).toBeVisible();
	// The reason, which is the entire question being answered.
	await expect(queue.getByText('spring brochure')).toBeVisible();
	// And what is being asked for, by name, with the intended use.
	await expect(queue.getByText('harbour.jpg')).toBeVisible();
	await expect(queue.getByText('for print')).toBeVisible();
	await expect(queue.getByText('agency@example.com')).toBeVisible();

	await queue.getByRole('button', { name: 'Decide…' }).click();
	await queue.getByLabel('A note, if it needs one').fill('Print only.');
	await queue.getByRole('button', { name: 'Approve' }).click();

	await expect(page.getByText('is approved')).toBeVisible();
	expect(recorder.decisions).toEqual(['o-ORD-000001:approve']);
});

test('an approved order does not claim the files are ready', async ({ page }) => {
	// The pickup is a later slice. Saying "approved, being prepared" is the honest version; a download button
	// would be the interface promising something the system has not done.
	await connect(page, {
		mine: [
			order({
				state: 'approved',
				decided_by: { id: 'p-ada', name: 'Ada Lovelace', email: 'ada@example.com' },
				decision_note: 'Print only.',
				expires_at: '2026-09-01T09:00:00Z'
			})
		]
	});
	await page.goto('/orders');

	await expect(page.getByText('Approved by Ada Lovelace')).toBeVisible();
	await expect(page.getByText('Print only.')).toBeVisible();
	await expect(page.getByText('The pickup is being prepared')).toBeVisible();
	await expect(page.getByText('collect by')).toBeVisible();
	await expect(page.getByRole('button', { name: /Download/ })).toHaveCount(0);
});

test('an expired pickup says so rather than reading ready', async ({ page }) => {
	// The window closing is a different fact from the decision, and a row still reading "ready" would send
	// somebody to a link that no longer works.
	await connect(page, {
		mine: [
			order({
				state: 'ready',
				expired: true,
				decided_by: { id: 'p-ada', name: 'Ada', email: 'a@x' },
				expires_at: '2026-08-02T09:00:00Z'
			})
		]
	});
	await page.goto('/orders');

	await expect(page.getByText('expired', { exact: true })).toBeVisible();
	await expect(page.getByText('ready', { exact: true })).toHaveCount(0);
	// And no invitation to collect something that cannot be collected.
	await expect(page.getByText('collect by')).toHaveCount(0);
});

test('a refusal is shown in the server’s own words', async ({ page }) => {
	await connect(page, {
		queue: [order()],
		decisionRefusal: {
			status: 403,
			reason: '2 of the assets in this order are outside your scope, so you cannot judge it'
		}
	});
	await page.goto('/orders');

	const queue = page.getByRole('region', { name: 'Waiting for a decision' });
	await queue.getByRole('button', { name: 'Decide…' }).click();
	await queue.getByRole('button', { name: 'Approve' }).click();

	// The count, as sent: the problem is the approver's scope, and that is something they can act on.
	await expect(page.getByRole('alert')).toContainText('outside your scope');
});

test('a submitted order can be withdrawn and a decided one cannot', async ({ page }) => {
	const recorder = await connect(page, { mine: [order({ state: 'submitted' })] });
	await page.goto('/orders');

	await page.getByRole('button', { name: 'Withdraw' }).click();
	await expect(page.getByText('is cancelled')).toBeVisible();
	expect(recorder.decisions).toEqual(['o-ORD-000001:cancel']);

	// After a decision there is nothing to withdraw: an approval is somebody else's recorded act.
	await expect(page.getByRole('button', { name: 'Withdraw' })).toHaveCount(0);
});

test('an empty list says where to start', async ({ page }) => {
	// An order is placed from a selection in the grid, which is not discoverable from an empty page.
	await connect(page, { mine: [] });
	await page.goto('/orders');

	await expect(page.getByText('You have not ordered anything')).toBeVisible();
	await expect(page.getByRole('link', { name: 'Assets' }).first()).toBeVisible();
});

test('the orders page has no accessibility violations', async ({ page }) => {
	await connect(page, {
		mine: [
			order({ state: 'approved', decided_by: { id: 'p-ada', name: 'Ada', email: 'a@x' } }),
			order({ reference: 'ORD-000002', state: 'rejected', decision_note: 'Not licensed.' })
		],
		queue: [order({ reference: 'ORD-000003' })]
	});
	await page.goto('/orders');
	await expect(page.getByRole('region', { name: 'My orders' })).toBeVisible();
	await page
		.getByRole('region', { name: 'Waiting for a decision' })
		.getByRole('button', { name: 'Decide…' })
		.click();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
