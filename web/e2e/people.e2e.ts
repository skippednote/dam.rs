/**
 * Who can use this library, on screen (G10·2a).
 *
 * The Rust suites prove the transitions, the last-administrator rule and that removal reaches the credential.
 * What only a browser can show:
 *
 * - **The key survives long enough to be saved.** It cannot be read back, so a panel that cleared itself would
 *   lock somebody out before they had it.
 * - **Roles are offered, not typed.** `role_names` has no foreign key and the server ignores a name it cannot
 *   resolve, so a text field is how "editors" happens.
 * - **An administrator with no roles is not described as having no access.** A tenant administrator gets every
 *   asset group without holding a role, and the first version of this screen said "can sign in and see
 *   nothing" beside four of them on the dev tenant.
 * - **Removal states what it revoked.** The count is the difference between an account that has been removed
 *   and one that has been marked removed.
 * - **A row the identity provider owns offers no role editing**, and says where the change belongs — an edit
 *   here would be overwritten on the next sync and reported as data loss.
 * - **Removal takes two deliberate actions**, and the confirmation says what will stop working.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Member = {
	identity_id: string;
	email: string;
	display_name: string | null;
	role_names: string[];
	is_tenant_admin: boolean;
	status: string;
	scim_managed: boolean;
	last_login_at: string | null;
	live_keys: number;
	joined_at: string;
};

function member(overrides: Partial<Member> = {}): Member {
	return {
		identity_id: 'aaaaaaaa-0000-4000-8000-000000000001',
		email: 'ada@example.com',
		display_name: 'Ada',
		role_names: ['editor'],
		is_tenant_admin: false,
		status: 'active',
		scim_managed: false,
		last_login_at: null,
		live_keys: 1,
		joined_at: '2026-08-01T09:00:00Z',
		...overrides
	};
}

type Options = {
	members?: Member[];
	roles?: string[];
	listStatus?: number;
	patchStatus?: number;
	patchReason?: string;
	removed?: { keys_revoked: number; identity_disabled: boolean };
};

async function connect(page: Page, options: Options = {}) {
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});
	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		if (url.pathname === '/roles') {
			return route.fulfill({ json: options.roles ?? ['editor', 'viewer'] });
		}
		if (url.pathname === '/members' && method === 'GET') {
			if (options.listStatus) {
				return route.fulfill({ status: options.listStatus, json: { reason: 'no' } });
			}
			return route.fulfill({ json: options.members ?? [member()] });
		}
		if (url.pathname === '/members' && method === 'POST') {
			return route.fulfill({
				status: 201,
				json: {
					identity_id: 'bbbbbbbb-0000-4000-8000-000000000002',
					api_key: 'damrs_0123456789abcdef',
					warning: 'This key is shown only here.'
				}
			});
		}
		if (url.pathname.startsWith('/members/') && method === 'PATCH') {
			if (options.patchStatus) {
				return route.fulfill({
					status: options.patchStatus,
					json: { reason: options.patchReason ?? 'refused' }
				});
			}
			return route.fulfill({ json: member({ role_names: ['viewer'] }) });
		}
		if (url.pathname.startsWith('/members/') && method === 'DELETE') {
			return route.fulfill({
				json: options.removed ?? { keys_revoked: 2, identity_disabled: true }
			});
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		return route.fulfill({ status: 404, json: {} });
	});
}

test('the issued key stays on screen until it is dismissed', async ({ page }) => {
	// It cannot be read back. A toast here is somebody locked out before they arrived.
	await connect(page);
	await page.goto('/people');
	await page.getByRole('button', { name: 'Add someone' }).click();
	await page.getByLabel('Email').fill('grace@example.com');
	await page.getByRole('button', { name: 'Add and issue a key' }).click();

	const panel = page.getByTestId('issued');
	await expect(panel).toContainText('grace@example.com can now sign in');
	await expect(panel.getByLabel('API key')).toHaveValue('damrs_0123456789abcdef');
	await expect(panel).toContainText('shown only here');

	await panel.getByRole('button', { name: 'I have saved it' }).click();
	await expect(panel).toHaveCount(0);
});

test('roles are checkboxes from the tenant, not a text field', async ({ page }) => {
	await connect(page, { roles: ['curator', 'editor', 'viewer'] });
	await page.goto('/people');
	await page.getByRole('button', { name: 'Add someone' }).click();
	for (const role of ['curator', 'editor', 'viewer']) {
		await expect(page.getByRole('checkbox', { name: role })).toBeVisible();
	}
});

test('a tenant with no roles says what adding somebody would mean', async ({ page }) => {
	await connect(page, { roles: [] });
	await page.goto('/people');
	await page.getByRole('button', { name: 'Add someone' }).click();
	await expect(page.getByText('This library has no roles yet')).toContainText(
		'can sign in and see nothing'
	);
});

test('an administrator with no roles is not described as having no access', async ({ page }) => {
	// A tenant administrator gets every asset group without holding a role. The first version of this screen
	// said "can sign in and see nothing" beside four of them.
	await connect(page, {
		members: [
			member({ email: 'admin@example.com', role_names: [], is_tenant_admin: true }),
			member({
				identity_id: 'cccccccc-0000-4000-8000-000000000003',
				email: 'nobody@example.com',
				role_names: [],
				is_tenant_admin: false
			})
		]
	});
	await page.goto('/people');
	await expect(page.getByTestId('member-admin@example.com')).toContainText(
		'Everything, as an administrator'
	);
	await expect(page.getByTestId('member-nobody@example.com')).toContainText(
		'can sign in and see nothing'
	);
});

test('a non-active account says the keys do not work', async ({ page }) => {
	// Authentication allowlists `active`, so this is not cosmetic.
	await connect(page, { members: [member({ status: 'disabled', live_keys: 3 })] });
	await page.goto('/people');
	await expect(page.getByTestId('member-ada@example.com')).toContainText(
		'Account disabled — their keys do not work'
	);
});

test('a row the identity provider owns cannot have its roles edited here', async ({ page }) => {
	await connect(page, { members: [member({ scim_managed: true })] });
	await page.goto('/people');
	const row = page.getByTestId('member-ada@example.com');
	await expect(row).toContainText('From your identity provider');
	await expect(row.getByRole('button', { name: 'Change roles' })).toHaveCount(0);
	await expect(row).toContainText('Change them there');
	// Removing them from this tenant is still offered: that is not editing the account the IdP owns, and
	// refusing it would leave somebody who has left in place until the IdP was fixed.
	await expect(row.getByRole('button', { name: 'Remove' })).toBeVisible();
});

test('removal takes two actions and the confirmation says what stops working', async ({ page }) => {
	await connect(page, { members: [member({ live_keys: 2 })] });
	await page.goto('/people');
	const row = page.getByTestId('member-ada@example.com');
	await row.getByRole('button', { name: 'Remove' }).click();
	await expect(row).toContainText('Their 2 keys will stop working immediately');

	await row.getByRole('button', { name: 'Remove them' }).click();
	await expect(page.getByRole('status')).toContainText('2 keys revoked');
	await expect(page.getByRole('status')).toContainText('their account is disabled');
});

test('a removal that revoked nothing says that instead of claiming a number', async ({ page }) => {
	await connect(page, {
		members: [member({ live_keys: 0 })],
		removed: { keys_revoked: 0, identity_disabled: false }
	});
	await page.goto('/people');
	const row = page.getByTestId('member-ada@example.com');
	await row.getByRole('button', { name: 'Remove' }).click();
	await row.getByRole('button', { name: 'Remove them' }).click();
	await expect(page.getByRole('status')).toContainText('They had no keys to revoke');
});

test('the only administrator is warned before being asked to step down', async ({ page }) => {
	await connect(page, {
		members: [member({ role_names: [], is_tenant_admin: true })],
		patchStatus: 409,
		patchReason: "this is the tenant's only administrator"
	});
	await page.goto('/people');
	const row = page.getByTestId('member-ada@example.com');
	await row.getByRole('button', { name: 'Change roles' }).click();
	await expect(row).toContainText('This is the only administrator');

	// And the server's refusal is shown rather than swallowed, because the warning is advice and the rule is
	// the rule.
	await row.getByRole('checkbox', { name: 'Administrator' }).uncheck();
	await row.getByRole('button', { name: 'Save' }).click();
	await expect(page.getByRole('alert')).toContainText('only administrator');
});

test('a caller without the gate is told, not shown an error', async ({ page }) => {
	await connect(page, { listStatus: 403 });
	await page.goto('/people');
	await expect(page.getByRole('status')).toContainText('needs administrator access');
	await expect(page.getByRole('alert')).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Add someone' })).toHaveCount(0);
});

test('the people screen has no accessibility violations', async ({ page }) => {
	await connect(page, {
		members: [
			member({ is_tenant_admin: true, role_names: [] }),
			member({
				identity_id: 'dddddddd-0000-4000-8000-000000000004',
				email: 'idp@example.com',
				scim_managed: true
			})
		]
	});
	await page.goto('/people');
	await expect(page.getByTestId('member-ada@example.com')).toBeVisible();
	await page.getByRole('button', { name: 'Add someone' }).click();
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
