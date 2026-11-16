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
	await expect(page.locator('[data-tool="write"]')).toHaveCount(1);
	await expect(page.locator('[data-tool="write"]')).toHaveAttribute('data-status', 'done');
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
