import { existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { test, expect, Stack } from './stack';

const FINAL = 'Wrote and verified hello.txt.';

test('joining after a failed run shows the error, including after reload', async ({ page }) => {
	const stack = new Stack();
	try {
		await stack.start('auto', true);
		await page.goto(stack.pair);
		await page.locator('[data-session-list]:visible a').first().click();
		const error = page.getByText('invalid request: Test account has no quota', { exact: true });
		await expect(error).toBeVisible();
		await page.reload();
		await expect(error).toBeVisible();
	} finally {
		await stack.stop();
	}
});

test('pairing from the link signs the device in and lists the session', async ({ page, auto }) => {
	await page.goto(auto.pair);
	const list = page.locator('[data-session-list]:visible').first();
	await expect(list.getByRole('link')).toHaveCount(1);
	// The secret does not stay in the address bar.
	expect(page.url()).not.toContain('pair=');
	// A reload keeps the device signed in: the cookie, not the link, is what counts now.
	await page.reload();
	await expect(list.getByRole('link')).toHaveCount(1);
});

test('the chat shows the run and keeps streaming into it', async ({ page, manual }) => {
	await page.goto(manual.pair);
	await page.locator('[data-session-list]:visible a').first().click();
	await expect(page.locator('[data-request="approval"]')).toBeVisible();
	await page.getByRole('button', { name: 'Allow once' }).click();
	await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
	await expect(page.locator('[data-request="approval"]')).toHaveCount(0);
	// The write and the read that follows it are told as one run, which opens on its steps.
	await page.getByRole('button', { name: /Created a file, read a file/ }).click();
	const steps = page.locator('dialog[open]');
	await expect(steps.getByRole('heading', { name: 'Created a file, read a file' })).toBeVisible();
	await expect(steps.locator('[data-tool="write"]')).toHaveAttribute('data-status', 'done');
	await steps.locator('[data-tool="write"]').click();
	await expect(steps.getByRole('heading', { name: 'Changes' })).toBeVisible();
	await steps.getByRole('button', { name: 'Back' }).click();
	await expect(steps.locator('[data-tool="read"]')).toHaveAttribute('data-status', 'done');
});

test('answering the approval on one tab closes it on the other', async ({ browser, manual }) => {
	const context = await browser.newContext();
	const first = await context.newPage();
	await first.goto(manual.pair);
	await first.locator('[data-session-list]:visible a').first().click();
	// The click navigates inside the app: wait for the session's address before copying it.
	await first.waitForURL(/\/s\/[^/]+$/);
	// The second tab shares the device (the cookie) and opens the same session.
	const second = await context.newPage();
	await second.goto(first.url());
	for (const tab of [first, second]) {
		await expect(tab.locator('[data-request="approval"]')).toBeVisible();
	}
	await first.getByRole('button', { name: 'Allow once' }).click();
	await expect(second.locator('[data-request="approval"]')).toHaveCount(0);
	await expect(second.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
	await context.close();
});

test('a link works once', async ({ browser, auto }) => {
	const mine = await browser.newContext();
	const page = await mine.newPage();
	await page.goto(auto.pair);
	await expect(page.locator('[data-session-list]:visible').first()).toBeVisible();

	const other = await browser.newContext();
	const stranger = await other.newPage();
	await stranger.goto(auto.pair);
	await expect(stranger.getByRole('heading', { name: 'That link did not work' })).toBeVisible();
	await expect(stranger.locator('[data-session-list]')).toHaveCount(0);

	// A new link from the server's own terminal does work, on the device that has none yet.
	await stranger.goto(await auto.newLink('l'));
	await expect(stranger.locator('[data-session-list]:visible').first()).toBeVisible();
	await mine.close();
	await other.close();
});

test('the model chip names the role the lead follows, and a role can be picked', async ({
	page,
	auto
}) => {
	await page.goto(auto.pair);
	await page.locator('[data-session-list]:visible a').first().click();
	// The scripted lead follows its role, which resolves to the scripted model.
	const chip = page.locator('[data-statusbar]').getByRole('button', { name: /^Model:/ });
	await expect(chip).toHaveAccessibleName(/^Model: @\w+ \(mock\/lead\)$/);
	await chip.click();
	const roles = page.getByRole('list', { name: 'Roles' });
	await expect(roles.getByRole('button', { name: /@default/ })).toBeVisible();
	await roles.getByRole('button', { name: /@default/ }).click();
	await expect(chip).toHaveAccessibleName('Model: @default (mock/lead)');
});

test('a new chat starts in a folder once the computer allows it', async ({ page }) => {
	const stack = new Stack();
	try {
		const work = join(stack.dir, 'work');
		mkdirSync(join(work, 'app', '.git'), { recursive: true });
		await stack.start('auto', false, ['--set', `remote.folders=['${work}']`]);
		await page.goto(stack.pair);
		await page.getByRole('button', { name: 'New chat', exact: true }).first().click();
		const sheet = page.getByRole('dialog', { name: 'New chat' });
		const folders = sheet.getByRole('list', { name: 'Folders' });
		await folders
			.getByRole('button', { name: /^Open / })
			.first()
			.click();
		await folders.getByRole('button', { name: /app/ }).first().click();
		await sheet.getByLabel('First message').fill('create hello');
		await sheet.getByRole('radio', { name: 'auto' }).click();
		await expect(sheet.getByText('This folder is not trusted')).toBeVisible();
		const seen = stack.heardSoFar();
		await sheet.getByRole('button', { name: 'Start', exact: true }).click();
		await expect(sheet.getByRole('button', { name: /Waiting for the computer/ })).toBeVisible();
		await stack.heardLine('which is not trusted', seen);
		stack.type('y');
		await page.waitForURL(/\/s\/[^/]+$/);
		await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
		expect(existsSync(join(work, 'app', 'hello.txt'))).toBe(true);
	} finally {
		await stack.stop();
	}
});
