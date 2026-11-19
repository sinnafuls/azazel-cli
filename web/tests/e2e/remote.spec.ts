import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test, expect, Stack, type Page } from './stack';

const FINAL = 'Wrote and verified hello.txt.';

/**
 * The script's closing text claims success whatever the tools did, so a test that waits for it also
 * checks that no tool card failed and that the file is on disk (the project is new for every stack).
 */
async function ranCleanly(page: Page, stack: Stack, dir = stack.project) {
	await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
	await expect(page.locator('[data-status="error"]')).toHaveCount(0);
	expect(stack.hello(dir)).toBe('hello from azazel\n');
}

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
	await ranCleanly(page, manual);
	await expect(page.locator('[data-request="approval"]')).toHaveCount(0);
	// The write and the read that follows it are told as one run, which opens in place in the
	// transcript into a card per call.
	const row = page.getByRole('button', { name: /Created a file, read a file/ });
	await expect(row).toHaveAttribute('aria-expanded', 'false');
	await row.click();
	await expect(row).toHaveAttribute('aria-expanded', 'true');
	const steps = page.locator('[data-transcript] [data-steps="2"]');
	const write = steps.locator('[data-tool="write"]');
	await expect(write).toHaveAttribute('data-status', 'done');
	// The card of the write names the file and shows what went into it, as an added line.
	await expect(write.getByRole('heading', { name: 'hello.txt' })).toBeVisible();
	await expect(write.locator('[data-line=add]')).toHaveCount(1);
	await expect(write.locator('[data-line=add]')).toContainText('hello from azazel');
	const read = steps.locator('[data-tool="read"]');
	await expect(read).toHaveAttribute('data-status', 'done');
	await expect(read.getByLabel('Output', { exact: true })).toContainText('hello from azazel');
	// Nothing of a tool call opens beside the chat any more.
	await expect(
		page.getByRole('complementary', { name: 'Agents and usage' }).locator('[data-tool]')
	).toHaveCount(0);
	// The row closes its cards again.
	await row.click();
	await expect(row).toHaveAttribute('aria-expanded', 'false');
	await expect(write).toBeHidden();
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
	await ranCleanly(second, manual);
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
	// The picker is a Command list: its roles are the options of the group headed "Roles".
	const roles = page.getByRole('group', { name: 'Roles' });
	await expect(roles.getByRole('option', { name: /@default/ })).toBeVisible();
	await roles.getByRole('option', { name: /@default/ }).click();
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
		await ranCleanly(page, stack, join(work, 'app'));
	} finally {
		await stack.stop();
	}
});

test('an earlier session is resumed from the sidebar and takes a message', async ({ page }) => {
	const stack = new Stack();
	try {
		// Earlier sessions are listed for the folders a browser may look into: the project is one.
		await stack.start('auto', false, ['--set', `remote.folders=['${join(stack.dir, 'project')}']`]);
		// The session ends and stays on disk; the one that continues it reads the script from its start.
		stack.endSession();
		writeFileSync(
			join(stack.dir, 'script.json'),
			JSON.stringify({ lead: [{ reply: { text: 'Picked up where we left off.' } }] })
		);
		await page.goto(stack.pair);
		await page.getByRole('button', { name: 'Resume a session' }).first().click();
		const sheet = page.getByRole('dialog', { name: 'Resume a session' });
		const seen = stack.heardSoFar();
		await sheet
			.getByRole('list', { name: 'Earlier sessions' })
			.getByRole('button', { name: /create hello/ })
			.click();
		// The folder of the earlier session is not trusted: the computer is asked, as for New chat.
		await stack.heardLine('which is not trusted', seen);
		stack.type('y');
		await page.waitForURL(/\/s\/[^/]+$/);
		await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
		const box = page.getByRole('combobox', { name: 'Message' });
		await box.fill('are you still there?');
		await box.press('Enter');
		await expect(
			page.getByRole('paragraph').filter({ hasText: 'Picked up where we left off.' })
		).toBeVisible();
	} finally {
		await stack.stop();
	}
});

/** Pairs, opens the only session and waits until its message box is there. */
async function openSession(page: Page, stack: Stack) {
	await page.goto(stack.pair);
	await page.locator('[data-session-list]:visible a').first().click();
	await page.waitForURL(/\/s\/[^/]+$/);
	await expect(page.getByRole('combobox', { name: 'Message' })).toBeVisible();
}

/** The settings window, opened with its shortcut. */
async function openSettings(page: Page) {
	await page.keyboard.press('Control+.');
	const win = page.locator('[role=dialog][data-settings]');
	await expect(win).toBeVisible();
	return win;
}

test('a slash command runs in the browser and never reaches the model', async ({ page, auto }) => {
	await openSession(page, auto);
	const box = page.getByRole('combobox', { name: 'Message' });
	const card = page.locator('[data-item="command"][data-command="cost"]');
	await box.fill('/cost');
	await box.press('Enter');
	await expect(card).toBeVisible();
	await expect(card).toContainText('requests:');
	// The script has no answer left: a message that reached the model would fail with an error.
	await expect(page.locator('[data-item="user"]', { hasText: '/' })).toHaveCount(0);
	await expect(page.getByText('mock script exhausted')).toHaveCount(0);

	// Typed the moment the page is there, before the list of commands has arrived.
	await page.reload();
	await box.fill('/cost');
	await box.press('Enter');
	await expect(card).toBeVisible();
	await expect(card).toContainText('requests:');
	await expect(page.locator('[data-item="user"]', { hasText: '/' })).toHaveCount(0);
	await expect(page.getByText('mock script exhausted')).toHaveCount(0);
	// The line left the box: it was run, not refused.
	await expect(box).toHaveValue('');
});

test('a lone /word that is no command is not sent', async ({ page, auto }) => {
	await openSession(page, auto);
	const box = page.getByRole('combobox', { name: 'Message' });
	await box.fill('/trust');
	await box.press('Escape');
	await box.press('Enter');
	await expect(
		page.locator('[data-sonner-toast]', { hasText: "/trust isn't a command" })
	).toBeVisible();
	await expect(box).toHaveValue('/trust');
	await expect(page.locator('[data-item="user"]', { hasText: '/' })).toHaveCount(0);
});

test('the settings window shows every pane, and a live change is kept for this session', async ({
	page,
	auto
}) => {
	await openSession(page, auto);
	const win = await openSettings(page);
	const tab = (name: string) => win.getByRole('tab', { name, exact: true });
	const pane = (id: string) => win.locator(`[data-pane="${id}"]`);

	await expect(tab('General')).toHaveAttribute('aria-selected', 'true');
	await expect(win.getByRole('heading', { name: 'Theme' })).toBeVisible();

	await tab('All settings').click();
	const search = win.getByRole('searchbox', { name: 'Search settings' });
	await expect(search).toBeVisible();
	// A background session applies only the theme for this session; other live settings are saved.
	await search.fill('tui.theme');
	const row = win.locator('[data-setting="tui.theme"]');
	await expect(row).toBeVisible();
	await expect(row.getByText('this session', { exact: true })).toHaveCount(0);
	const choice = row.locator('select');
	const next = await choice.evaluate((s: HTMLSelectElement) =>
		[...s.options].map((o) => o.value).find((v) => v !== s.value)
	);
	expect(next).toBeTruthy();
	await choice.selectOption(next!);
	await expect(row.getByText('this session', { exact: true })).toBeVisible();
	await expect(choice).toHaveValue(next!);

	await tab('Usage').click();
	await expect(pane('usage').locator('[data-cost]')).toBeVisible();
	await tab('Context').click();
	await expect(pane('context').locator('[data-context-used]')).toBeVisible();
	await tab('Devices and remote').click();
	const devices = pane('devices').getByRole('list', { name: 'Paired devices' });
	await expect(devices.getByText('this one', { exact: true })).toBeVisible();
	await expect(devices.getByText(/Full control/)).toBeVisible();
	await tab('About').click();
	await expect(pane('about').locator('[data-about]').first()).toBeVisible();

	await page.keyboard.press('Escape');
	await expect(win).toBeHidden();
});

test('the sidebar folds with its shortcut and its button, and stays folded after a reload', async ({
	page,
	auto
}) => {
	await openSession(page, auto);
	// The shadcn Sidebar says whether it is folded to its rail of icons in `data-state`.
	const side = page.locator('[data-slot="sidebar"][data-state]');
	await expect(side).toHaveAttribute('data-state', 'expanded');

	await page.keyboard.press('Control+Shift+S');
	await expect(side).toHaveAttribute('data-state', 'collapsed');
	await page.keyboard.press('Control+Shift+S');
	await expect(side).toHaveAttribute('data-state', 'expanded');

	await side.getByRole('button', { name: 'Hide the sidebar' }).click();
	await expect(side).toHaveAttribute('data-state', 'collapsed');
	await page.reload();
	await expect(page.getByRole('combobox', { name: 'Message' })).toBeVisible();
	await expect(side).toHaveAttribute('data-state', 'collapsed');

	await side.getByRole('button', { name: 'Show the sidebar' }).click();
	await expect(side).toHaveAttribute('data-state', 'expanded');
	await page.reload();
	await expect(page.getByRole('combobox', { name: 'Message' })).toBeVisible();
	await expect(side).toHaveAttribute('data-state', 'expanded');
});

test('a view link watches: no way to send, and settings stay read-only', async ({
	browser,
	auto
}) => {
	const view = await browser.newContext();
	try {
		const page = await view.newPage();
		let asked = 0;
		page.on('websocket', (ws) =>
			ws.on('framesent', (f) => {
				if (typeof f.payload === 'string' && f.payload.includes('"get_settings"')) asked++;
			})
		);
		await page.goto(await auto.newLink('v'));
		await page.locator('[data-session-list]:visible a').first().click();
		await page.waitForURL(/\/s\/[^/]+$/);
		// The run is visible, the message box is there but shut.
		await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toBeVisible();
		const box = page.getByRole('combobox', { name: 'Message' });
		await expect(box).toBeDisabled();
		await expect(box).toHaveAttribute('placeholder', 'View link: you can watch but not send');

		const win = await openSettings(page);
		await win.getByRole('tab', { name: 'All settings' }).click();
		const none = win.locator('[data-pane="all"] [data-unavailable]');
		await expect(none).toContainText('not available from this device');
		await expect(none).not.toContainText('view link');
		await expect(win.getByLabel('Where a change is saved')).toHaveCount(0);
		await win.getByRole('tab', { name: 'Devices and remote' }).click();
		const devices = win.locator('[data-pane="devices"] [data-unavailable]');
		await expect(devices).toContainText('not available from this device');
		await expect(devices).not.toContainText('view link');
		await expect(win.getByRole('button', { name: /Sign everyone out/ })).toHaveCount(0);
		// A refused read is not asked again when the pane is opened again.
		await win.getByRole('tab', { name: 'All settings' }).click();
		await expect(none).toBeVisible();
		expect(asked).toBeLessThanOrEqual(1);
	} finally {
		await view.close();
	}
});
