import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test, expect, Stack, agentBrowserFound, browses, serve, type Page } from './stack';

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
		page.getByRole('complementary', { name: 'Agents, usage and browser' }).locator('[data-tool]')
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

test('a profile is switched from its chip, and a view link only reads it', async ({
	browser,
	page
}) => {
	const stack = new Stack();
	// Two profiles for the scripted provider; the built-in ones need logins this home lacks.
	stack.config = [
		'[profiles.mine.roles]',
		'default = ["mock/lead"]',
		'smol = ["mock/scout"]',
		'',
		'[profiles.other.roles]',
		'default = ["mock/scout"]',
		''
	].join('\n');
	try {
		await stack.start('auto');
		await openSession(page, stack);
		const chip = page.locator('[data-statusbar]').getByRole('button', { name: /^Profile:/ });
		await expect(chip).toHaveAccessibleName('Profile: none');
		await chip.click();
		const menu = page.getByRole('menu');
		const item = (name: string) => menu.locator(`[role=menuitemradio][data-profile="${name}"]`);
		// Each row: the name, where it comes from, its models; one without a login cannot be chosen.
		await expect(item('mine')).toContainText('your config');
		await expect(item('mine')).toContainText('lead · scout');
		await expect(item('claude')).toContainText('built-in');
		await expect(item('claude')).toHaveAttribute('aria-disabled', 'true');
		await expect(item('claude')).toContainText('needs azazel login anthropic');
		await item('mine').click();
		await expect(menu).toHaveCount(0);
		await expect(chip).toHaveAccessibleName('Profile: mine');
		// A switch somebody chose says nothing.
		await expect(page.locator('[data-sonner-toast]')).toHaveCount(0);
		await chip.click();
		await expect(item('mine')).toHaveAttribute('aria-checked', 'true');
		await page.keyboard.press('Escape');

		// The settings list the same profiles; without full access nothing can be saved.
		const win = await openSettings(page);
		await win.getByRole('tab', { name: 'Models and roles', exact: true }).click();
		const card = win.locator('[data-profiles]');
		await expect(card.locator('[data-profile="mine"]')).toHaveAttribute('aria-current', 'true');
		await expect(win.getByLabel('Save current as…')).toHaveCount(0);
		await expect(win.locator('[data-profile-setting="on_reset"]')).toContainText('ask');
		await card.getByRole('button', { name: 'Switch to the profile other' }).click();
		await expect(card.locator('[data-profile="other"]')).toHaveAttribute('aria-current', 'true');
		await page.keyboard.press('Escape');
		await expect(chip).toHaveAccessibleName('Profile: other');
		// The computer keeps it: a reload reads the same profile back.
		await page.reload();
		await expect(chip).toHaveAccessibleName('Profile: other');

		// `/profile <name>` switches too, and never reaches the model.
		const box = page.getByRole('combobox', { name: 'Message' });
		await box.fill('/profile mine');
		await box.press('Enter');
		await expect(chip).toHaveAccessibleName('Profile: mine');
		await expect(page.getByRole('paragraph').filter({ hasText: FINAL })).toHaveCount(1);

		// A view device sees which profile the session is on, and cannot open the list.
		const view = await browser.newContext();
		try {
			const watcher = await view.newPage();
			await watcher.goto(await stack.newLink('v'));
			await watcher.locator('[data-session-list]:visible a').first().click();
			await watcher.waitForURL(/\/s\/[^/]+$/);
			const seen = watcher.locator('[data-statusbar]').getByRole('button', { name: /^Profile:/ });
			await expect(seen).toHaveAccessibleName('Profile: mine');
			await expect(seen).toBeDisabled();
		} finally {
			await view.close();
		}
	} finally {
		await stack.stop();
	}
});

test('a slash command runs in the browser and never reaches the model', async ({ page, auto }) => {
	await openSession(page, auto);
	const box = page.getByRole('combobox', { name: 'Message' });
	const card = page.locator('[data-item="command"][data-command="cost"]');
	await box.fill('/cost');
	// The Send button, not Enter: a pointer or a tap on it must submit as Enter does.
	await page.getByRole('button', { name: 'Send', exact: true }).click();
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

// The sheet is a Dialog from 768px and a Drawer below; both give the focus back to what opened them.
for (const width of [900, 600])
	test(`focus that comes back to a button opens no tooltip, Tab and the pointer do (${width}px)`, async ({
		page,
		auto
	}) => {
		await page.setViewportSize({ width, height: 700 });
		await openSession(page, auto);
		const button = page.getByRole('button', { name: 'Agents, usage and browser' });
		const tip = page.locator('[data-slot=tooltip-content]');
		// The button before it in the header has a tooltip of its own, which Tab opens on the way.
		const own = tip.filter({ hasText: 'Agents, usage and browser' });
		const sheet = page.getByRole('dialog', { name: 'Agents' });
		// The pointer rests where no tooltip opens: what opens one below is the focus alone.
		await page.mouse.move(2, 698);

		// Focus a script gave says nothing.
		await button.focus();
		await expect(button).toBeFocused();
		await expect(tip).toHaveCount(0);

		// The sheet, opened and closed from the keyboard: the focus is back on the button, silently.
		await page.keyboard.press('Enter');
		await expect(sheet).toBeVisible();
		await page.keyboard.press('Escape');
		await expect(sheet).toHaveCount(0);
		await expect(button).toBeFocused();
		await expect(button).toHaveAttribute('data-state', 'closed');
		await expect(tip).toHaveCount(0);

		// Closed with its button, the same.
		await page.keyboard.press('Enter');
		await expect(sheet).toBeVisible();
		await sheet.getByRole('button', { name: 'Close' }).click();
		await expect(sheet).toHaveCount(0);
		await page.mouse.move(2, 698);
		await expect(button).toBeFocused();
		await expect(tip).toHaveCount(0);

		// Reached with Tab, the button is named by its tooltip, which leaves with the focus.
		await page.keyboard.press('Shift+Tab');
		await expect(button).not.toBeFocused();
		await expect(own).toHaveCount(0);
		await page.keyboard.press('Tab');
		await expect(button).toBeFocused();
		await expect(own).toBeVisible();
		await page.keyboard.press('Tab');
		await expect(button).not.toBeFocused();
		await expect(own).toHaveCount(0);

		// And so does the pointer.
		await button.hover();
		await expect(own).toBeVisible();
	});

/** A page that is one button: a click anywhere in it changes its title, its text and its colour. */
const CLICKABLE = `<!doctype html>
<html lang="en">
	<head>
		<meta charset="utf-8" />
		<title>Before the click</title>
		<style>
			html, body { height: 100%; margin: 0; }
			button { width: 100%; height: 100%; border: 0; background: #fff; font: 32px system-ui, sans-serif; }
		</style>
	</head>
	<body>
		<button onclick="document.title = 'Clicked'; this.textContent = 'Clicked'; this.style.background = '#bfdbfe'">
			Press anywhere
		</button>
	</body>
</html>`;

/**
 * A page with one small button in its lower right corner, at any size of the window: the title
 * says whether a click was on the button or beside it, and how large the window was then.
 */
const CORNER = `<!doctype html>
<html lang="en">
	<head>
		<meta charset="utf-8" />
		<title>Not clicked</title>
		<style>
			html, body { height: 100%; margin: 0; background: #fff; }
			button { position: fixed; right: 0; bottom: 0; width: 30vw; height: 20vh; border: 0; background: #2563eb; }
		</style>
	</head>
	<body onclick="document.title = (event.target.tagName === 'BUTTON' ? 'Hit ' : 'Missed ') + innerWidth + 'x' + innerHeight">
		<button aria-label="Corner"></button>
	</body>
</html>`;

/** A page that is one field: its title says what is in the field, and that Enter was pressed. */
const TYPABLE = `<!doctype html>
<html lang="en">
	<head>
		<meta charset="utf-8" />
		<title>Nothing typed</title>
		<style>
			html, body { height: 100%; margin: 0; }
			input { box-sizing: border-box; width: 100%; height: 100%; border: 0; font: 32px system-ui, sans-serif; }
		</style>
	</head>
	<body>
		<input
			aria-label="Name"
			oninput="document.title = 'typed:' + this.value"
			onkeydown="if (event.key === 'Enter') document.title = 'entered:' + this.value"
		/>
	</body>
</html>`;

test('taking over, what is typed and what an input method composes reaches the page and stays nowhere else', async ({
	page
}) => {
	test.skip(!agentBrowserFound, 'no Chromium-based browser on this machine for the agent to open');
	const site = await serve(TYPABLE);
	const stack = new Stack();
	try {
		await stack.start('auto', false, [], browses(site.url, 'Opened the page.'));
		await openSession(page, stack);
		await expect(page.getByRole('paragraph').filter({ hasText: 'Opened the page.' })).toBeVisible();

		const pane = page.getByRole('complementary', { name: 'Agents, usage and browser' });
		await pane.getByRole('tab', { name: 'Browser' }).click();
		const panel = pane.locator('[data-browser]');
		const title = panel.locator('[data-browser-title]');
		const picture = panel.locator('[data-browser-picture]');
		await expect(title).toHaveText('Nothing typed');
		await expect
			.poll(() => picture.getByRole('img').evaluate((el: HTMLImageElement) => el.naturalWidth))
			.toBeGreaterThan(0);

		await panel.getByRole('button', { name: 'Take over' }).click();
		await expect(panel.getByRole('status')).toHaveText('Holding');
		// The keyboard is on a field in the picture, which is a field so that a phone shows its
		// keyboard; it takes no room and nothing of it shows.
		const keys = picture.getByRole('textbox', { name: "Type into the agent's page" });
		await expect(keys).toBeFocused();
		const drawn = await keys.evaluate((el) => {
			const r = el.getBoundingClientRect();
			const s = getComputedStyle(el);
			return { width: r.width, height: r.height, opacity: s.opacity, position: s.position };
		});
		expect(drawn).toEqual({ width: 1, height: 1, opacity: '0', position: 'absolute' });

		// A click puts the caret in the page's field, and the keyboard stays on the picture.
		await picture.click();
		await expect(keys).toBeFocused();
		// Keys of the layout, and characters that come as text without a key of their own.
		await page.keyboard.type('héllo wörld');
		await expect(title).toHaveText('typed:héllo wörld');
		await expect(keys).toHaveValue('');
		// A key that is no character is still a key.
		await page.keyboard.press('Backspace');
		await expect(title).toHaveText('typed:héllo wörl');
		await page.keyboard.press('Enter');
		await expect(title).toHaveText('entered:héllo wörl');

		// An input method: nothing goes to the page while it composes, and what it made goes once.
		const cdp = await page.context().newCDPSession(page);
		await cdp.send('Input.imeSetComposition', { text: 'にほ', selectionStart: 2, selectionEnd: 2 });
		await expect(keys).toHaveValue('にほ');
		await cdp.send('Input.insertText', { text: '日本' });
		await expect(title).toHaveText('typed:héllo wörl日本');
		await expect(keys).toHaveValue('');

		// Nothing of it went anywhere else on this page: it shows only where the agent's page itself
		// says what its field holds, in its title.
		await expect(page.getByRole('combobox', { name: 'Message' })).toHaveText('');
		await panel.getByRole('button', { name: 'Hand back' }).click();
		await expect(panel.getByRole('status')).toHaveText('Watching');
		await expect(keys).toHaveCount(0);
		await expect(title).toHaveText('typed:héllo wörl日本');
		const elsewhere = await page.evaluate(() => {
			const copy = document.body.cloneNode(true) as HTMLElement;
			copy.querySelector('[data-browser-title]')?.remove();
			const fields = Array.from(document.querySelectorAll('input, textarea'));
			return [copy.textContent, ...fields.map((f) => (f as HTMLInputElement).value)].join('\n');
		});
		expect(elsewhere).not.toMatch(/héllo|wörl|日本|にほ/);
	} finally {
		await stack.stop();
		await site.close();
	}
});

test("the Browser tab shows the agent's browser; taking over, a click reaches its page", async ({
	page
}) => {
	test.skip(!agentBrowserFound, 'no Chromium-based browser on this machine for the agent to open');
	const site = await serve(CLICKABLE);
	const stack = new Stack();
	try {
		await stack.start('auto', false, [], browses(site.url, 'Opened the page.'));
		await openSession(page, stack);
		await expect(page.getByRole('paragraph').filter({ hasText: 'Opened the page.' })).toBeVisible();
		await expect(page.locator('[data-status="error"]')).toHaveCount(0);

		const pane = page.getByRole('complementary', { name: 'Agents, usage and browser' });
		await pane.getByRole('tab', { name: 'Browser' }).click();
		const panel = pane.locator('[data-browser]');
		const status = panel.getByRole('status');
		const title = panel.locator('[data-browser-title]');
		const picture = panel.locator('[data-browser-picture]');
		const img = picture.getByRole('img');

		// Watching: where the browser is, and a picture of it that has loaded.
		await expect(status).toHaveText('Watching');
		await expect(title).toHaveText('Before the click');
		await expect(panel.locator('[data-browser-url]')).toHaveText(site.url);
		await expect(img).toBeVisible();
		await expect
			.poll(() => img.evaluate((el: HTMLImageElement) => el.naturalWidth))
			.toBeGreaterThan(0);
		const before = await img.getAttribute('src');
		expect(before).toMatch(/^data:image\/jpeg;base64,/);

		// Taken over: the address can be typed, and a click on the picture is a click in the page.
		await panel.getByRole('button', { name: 'Take over' }).click();
		await expect(status).toHaveText('Holding');
		await expect(panel.getByRole('textbox', { name: 'Address' })).toHaveValue(site.url);
		await picture.click();
		await expect(title).toHaveText('Clicked');
		// The page changed colour, so the picture is another one.
		await expect.poll(() => img.getAttribute('src')).not.toBe(before);

		// Handed back: watching again, with nothing to type into.
		await panel.getByRole('button', { name: 'Hand back' }).click();
		await expect(status).toHaveText('Watching');
		await expect(panel.getByRole('textbox', { name: 'Address' })).toHaveCount(0);
		await expect(panel.getByRole('button', { name: 'Take over' })).toBeEnabled();
		await expect(title).toHaveText('Clicked');
	} finally {
		await stack.stop();
		await site.close();
	}
});

test('the picture fills its room at any size of the window, and a click lands where it points', async ({
	page
}) => {
	test.skip(!agentBrowserFound, 'no Chromium-based browser on this machine for the agent to open');
	const site = await serve(CORNER);
	const stack = new Stack();
	try {
		await stack.start('auto', false, [], browses(site.url, 'Opened the page.'));
		await openSession(page, stack);
		await expect(page.getByRole('paragraph').filter({ hasText: 'Opened the page.' })).toBeVisible();

		const pane = page.getByRole('complementary', { name: 'Agents, usage and browser' });
		await pane.getByRole('tab', { name: 'Browser' }).click();
		const panel = pane.locator('[data-browser]');
		const title = panel.locator('[data-browser-title]');
		const picture = panel.locator('[data-browser-picture]');
		const img = picture.getByRole('img');
		/** The room of the picture, and the bars a picture of another shape leaves in it. */
		const room = () =>
			picture.evaluate((el) => {
				const r = el.getBoundingClientRect();
				const i = el.querySelector('img');
				if (!i?.naturalWidth) return { width: r.width, height: r.height, bars: Infinity };
				const scale = Math.min(r.width / i.naturalWidth, r.height / i.naturalHeight);
				return {
					width: r.width,
					height: r.height,
					bars: Math.max(r.width - i.naturalWidth * scale, r.height - i.naturalHeight * scale)
				};
			});
		const filled = async () => {
			await expect
				.poll(async () => (await room()).bars, { timeout: 20_000 })
				.toBeLessThanOrEqual(2);
			return room();
		};
		/** Clicks the picture at fractions of its room, and returns what the page's title says. */
		const clickAt = async (fx: number, fy: number, said: RegExp) => {
			const r = await room();
			await picture.click({ position: { x: r.width * fx, y: r.height * fy } });
			await expect(title).toHaveText(said);
			return (await title.textContent()) ?? '';
		};
		/** The size the page of the agent has, as its title says after a click. */
		const sizeIn = (text: string) => text.split(' ')[1].split('x').map(Number);

		await expect(title).toHaveText('Not clicked');
		await expect(img).toBeVisible();
		await panel.getByRole('button', { name: 'Take over' }).click();
		await expect(panel.getByRole('status')).toHaveText('Holding');

		// The page is laid out for the room: it has the room's shape, and the button in its corner
		// is under the corner of the picture, the rest of the page under the rest.
		const first = await filled();
		const hit = await clickAt(0.9, 0.95, /^Hit /);
		const [w1, h1] = sizeIn(hit);
		expect(Math.abs(w1 / h1 - first.width / first.height)).toBeLessThan(0.02);
		await clickAt(0.5, 0.5, /^Missed /);
		await clickAt(0.9, 0.7, /^Missed /);

		// The window gets taller and narrower: the room does, and then the page. A click in the new
		// corner is on the button, and one where the button was before is beside it.
		await page.setViewportSize({ width: 1100, height: 1300 });
		await expect.poll(async () => (await room()).height).toBeGreaterThan(first.height + 200);
		const second = await filled();
		const again = await clickAt(0.9, 0.95, /^Hit /);
		const [w2, h2] = sizeIn(again);
		expect([w2, h2]).not.toEqual([w1, h1]);
		expect(Math.abs(w2 / h2 - second.width / second.height)).toBeLessThan(0.02);
		await clickAt(0.9, 0.78, /^Missed /);
		await clickAt(0.65, 0.95, /^Missed /);
		await clickAt(0.75, 0.85, /^Hit /);
	} finally {
		await stack.stop();
		await site.close();
	}
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
