// The layout, measured. Every state of the page is opened at five sizes and once zoomed to 200%, in
// both themes, and what the browser laid out is read back with getBoundingClientRect and
// getComputedStyle:
//
// - centre: in every row (button, link, menu item, option, tab, badge) that lays its parts out in a
//   line, each part's vertical middle is within 1px of the row's middle and of every other part's;
//   in any other box that centres its children in a line, the children are within 1px of each other;
// - sidebar: the title, the state dot, the count and the ••• button of a sidebar row are within 1px
//   of the middle of the row's button, and the title ends before the ••• button. In a row of two
//   lines (a title over its folder) the two lines together are what is centred;
// - head: the first line of a sheet's title is within 1px of the middle of the buttons beside it;
// - item: the mark of an item of several lines (an agent's state dot) stands at the top, within 1px
//   of the middle of the title's line;
// - uniform: controls of one kind (buttons by size, inputs, selects, menu items, options, sidebar
//   buttons, tabs of one list, badges, switches) have one height and one corner radius;
// - clip, spill, cut, scroll, viewport: text wider than its box is only ever hidden behind an
//   ellipsis, nothing reaches outside the page or past a box that clips it, no line shows past
//   the lines a box is clamped to, and only code, diffs and tables scroll sideways, inside their
//   own box;
// - overlap: the parts of a row do not cover each other;
// - font: text is set in Geist, code in Geist Mono, and both fonts are loaded.
//
// What rectangles cannot say, and so is not measured here:
// - where the ink of a glyph sits inside its line box. A text's box is its line box, which is what
//   is centred; the screenshots saved beside the measurements are for that.
// - colours: the contrast of the tokens is checked in src/lib/theme.test.ts.
// - the fill of a meter (the shadcn Progress) is as wide as its track and slid out of it by what
//   is not used, so the part its track hides is not a finding.
// - a row that wraps onto several lines has a middle per line, so its parts are not compared with
//   the middle of the whole row. Segments of a joined toggle group round only their outer corners,
//   so only their heights are compared.
//
// Zoomed to 200%, a window of 1440x900 is a viewport of 720x450 CSS pixels drawn at two device
// pixels each: that is what the browser's own zoom does, so it is what is emulated (a viewport of
// that size with deviceScaleFactor 2). Media queries, `dvh` and `matchMedia` then answer as they do
// in a zoomed window, and the page takes its phone layout as it does there. CSS `zoom` on <html>
// would leave all three at 1440x900 and scale the rectangles that are measured here.
//
// What starts below the fold of a sheet (the last agent of the list, the last choice of the rewind
// sheet: both do in the zoomed window) is scrolled into view and measured there: all of it is inside
// the part of the sheet that scrolls, above the footer, and a pointer on it lands on it. Where each
// was before and after is in reach.json, beside measured.json.
//
// Screenshots and the measurements of every state go to tmp/layout, or to AZAZEL_LAYOUT_SHOTS;
// those of the profiles (their chip, menu, settings and the notice of a usage limit) to a folder of
// their own beside each size's, `<size>-<theme>-profiles`; those of the Browser tab (no browser,
// waiting for the first picture, watching a page, holding it) to `<size>-<theme>-browser`, with the
// boxes of its parts in boxes.json. All but the first need a browser for the agent to open: without
// one on the machine only the first is measured, and the test says so in an annotation.
import { mkdirSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import type { BrowserContext, Locator } from '@playwright/test';
import { test, expect, Stack, agentBrowserFound, browses, serve, type Page } from './stack';

const SHOTS =
	process.env.AZAZEL_LAYOUT_SHOTS ??
	join(resolve(import.meta.dirname, '../../..'), 'tmp', 'layout');

interface View {
	/** The window, as the test is named and its folder is called. */
	name: string;
	/** The viewport in CSS pixels, and how many device pixels each is drawn with. */
	width: number;
	height: number;
	scale: number;
}
const VIEWS: View[] = [
	...[
		[390, 844],
		[768, 1024],
		[1024, 768],
		[1440, 900],
		[1920, 1080]
	].map(([width, height]) => ({ name: `${width}x${height}`, width, height, scale: 1 })),
	{ name: '1440x900-zoom200', width: 720, height: 450, scale: 2 }
];
const THEMES = ['dark', 'light'] as const;

/** Declares a test of `view`: a zoomed one gets a browser that draws every CSS pixel `scale` times. */
function at(view: View, title: string, body: Parameters<typeof test>[2]) {
	if (view.scale === 1) return test(title, body);
	test.describe(() => {
		test.use({
			viewport: { width: view.width, height: view.height },
			deviceScaleFactor: view.scale
		});
		test(title, body);
	});
}
/** The accents of Settings > General, the default one last so that it is the one left chosen. */
const ACCENTS = [
	['blue', 'Blue'],
	['violet', 'Violet'],
	['rose', 'Rose'],
	['orange', 'Orange'],
	['amber', 'Amber'],
	['neutral', 'Neutral'],
	['emerald', 'Emerald']
] as const;

const MARKDOWN = [
	'## All done',
	'',
	'Wrote `hello.txt` and read it back. The **greeting** is one line, and [the docs](https://example.com/docs) say more.',
	'',
	'- one file created',
	'- one file read',
	'',
	'```ts',
	"export const greeting: string = 'hello from azazel, in a line that is far too long for a phone and has to scroll';",
	'console.log(greeting);',
	'```',
	'',
	'| File | Lines |',
	'| --- | --- |',
	'| hello.txt | 1 |'
].join('\n');

/** A file long enough to scroll inside its card, with a line too wide for a phone. */
const WELCOME = [
	'<script lang="ts">',
	'\t// The first screen: what the page is for, in a line that is far too long for a phone and has to scroll.',
	...Array.from({ length: 26 }, (_, i) => `\tconst line${i + 1} = ${i + 1};`),
	'</script>',
	'',
	'<h1>Welcome</h1>'
].join('\n');

const UNVERIFIED = 'Nothing was run after the write, so the change is unverified.';

/**
 * The session on screen: a write that asks first and a read (one run), two commands, the second
 * with more output than its card shows at once (a second run), a read that fails (a third), then
 * an answer with every kind of block.
 */
const MAIN = {
	lead: [
		{
			reply: {
				tool_calls: [
					{ id: 'c1', name: 'write', input: { path: 'src/Welcome.svelte', content: WELCOME } }
				]
			}
		},
		{
			reply: {
				tool_calls: [
					{ id: 'c2', name: 'read', input: { path: 'src/Welcome.svelte' } },
					{
						id: 'c6',
						name: 'todo',
						input: {
							items: [
								{ text: 'Write the welcome screen', status: 'done' },
								{ text: 'Check it from the shell', status: 'in_progress' },
								{ text: 'Say what was done', status: 'pending' }
							]
						}
					}
				]
			}
		},
		{
			reply: {
				text: 'Wrote it. Checking from the shell.',
				tool_calls: [
					{ id: 'c3', name: 'shell', input: { command: 'echo "hello from the shell"' } },
					{
						id: 'c4',
						name: 'shell',
						input: {
							command:
								'for i in $(seq 1 60); do echo "line $i of the output, which goes on for longer than a narrow card is wide"; done'
						}
					}
				]
			}
		},
		{
			reply: {
				text: 'One more look.',
				tool_calls: [{ id: 'c5', name: 'read', input: { path: 'missing.txt' } }]
			}
		},
		{ reply: { text: MARKDOWN } },
		// The write is followed by a note that nothing has checked it (the done check), which is a
		// request of its own; then come the prompts sent from the page (see the rewind sheet).
		{ reply: { text: UNVERIFIED } },
		{ reply: { text: 'Noted.' } },
		{ reply: { text: 'Noted, again.' } }
	]
};
/** A prompt too long for its row in the rewind sheet, and for three lines of its quote. */
const LONG_PROMPT =
	'Now the same for the settings page, and keep the copy short: one line per setting, no jargon, and say what happens when it is switched off. Where a setting needs the server to start again, say that too, in the same line, and do not repeat it in a tooltip or anywhere else on the page.';
const DONE = { lead: [{ reply: { text: 'Done.' } }] };
/** A session at work: a read that fails, then a call that outlives the test (`test_wait`). */
const BUSY = {
	lead: [
		{ reply: { tool_calls: [{ id: 'c1', name: 'read', input: { path: 'missing.txt' } }] } },
		{
			reply: {
				text: 'That file is not there. Waiting for it.',
				tool_calls: [{ id: 'c2', name: 'test_wait', input: { ms: 600_000 } }]
			}
		}
	]
};

const ROUTES = 'api-routes';
const CALLERS = 'callers';
/** As long as the name of an agent may be. */
const LONG_AGENT = 'migrate-the-settings-store-to-the-new-schema-v2';
/**
 * A lead with a team: two coders at work (one waits for the scout it started, one for a command
 * that outlives the test), that scout, which is never answered, and a scout that is done. Every
 * agent is answered from a queue of its own, so none takes another's step.
 */
const TEAM = {
	lead: [
		{
			reply: {
				text: 'Three hands for this.',
				tool_calls: [
					{
						id: 't1',
						name: 'task',
						input: {
							agents: [
								{
									agent: 'coder',
									name: ROUTES,
									model: 'mock/routes',
									prompt: 'Move the routes of the API into a module of their own.'
								},
								{
									agent: 'coder',
									name: LONG_AGENT,
									model: 'mock/store',
									prompt: 'Move the settings store to the new schema and run its tests.'
								},
								{
									agent: 'scout',
									name: 'docs',
									model: 'mock/docs',
									prompt: 'Find where the docs name the routes.'
								}
							]
						}
					}
				]
			}
		}
	],
	routes: [
		{
			reply: {
				text: 'First, who calls them.',
				tool_calls: [
					{
						id: 'r1',
						name: 'task',
						input: {
							agents: [
								{
									agent: 'scout',
									name: CALLERS,
									model: 'mock/callers',
									prompt: 'List every caller of the routes.'
								}
							]
						}
					}
				]
			}
		}
	],
	store: [
		{
			reply: {
				text: 'Running the tests of the store.',
				tool_calls: [
					{
						id: 's1',
						name: 'test_wait',
						// The roster shows a call by its command: one too long for its line.
						input: {
							ms: 600_000,
							command: 'cargo test --package settings-store --lib -- schema::migrate --nocapture'
						}
					}
				]
			}
		}
	],
	docs: [
		{
			reply: {
				text: 'docs/api.md names every route, in the table under "Routes".',
				usage: {
					input: 18_400,
					output: 620,
					cache_read: 0,
					cache_write: 0,
					reasoning: 0,
					cost_usd: 0.0312
				}
			}
		}
	],
	callers: [{ delay_ms: 600_000, reply: { text: 'Never said.' } }]
};

interface Measured {
	rows: number;
	actions: number;
	/** Marks of items of several lines, measured against the line of their title. */
	marks: number;
	kinds: number;
	texts: number;
	findings: string[];
}

/** Runs in the page: measures everything under `scope` and returns what does not hold. */
function audit(scope: string): Measured {
	const found = new Set<string>();
	const roots = Array.from(document.querySelectorAll<HTMLElement>(scope));
	const all = roots.flatMap((r) => [r, ...Array.from(r.querySelectorAll<HTMLElement>('*'))]);
	const view = document.documentElement.clientWidth;
	const css = (el: Element) => getComputedStyle(el);
	const rect = (el: Element) => el.getBoundingClientRect();
	const mid = (r: DOMRect) => r.top + r.height / 2;
	const px = (n: number) => String(Math.round(n * 100) / 100);
	// Laid out and not hidden. An element at opacity 0 (a ••• before the row is hovered) counts: its
	// place is the same. A box under 2px (a screen-reader text, a hairline) has nothing to align.
	const shown = (el: Element) => {
		const r = rect(el);
		return r.width >= 2 && r.height >= 2 && el.checkVisibility({ checkVisibilityCSS: true });
	};
	const name = (el: Element) => {
		const kind =
			el.getAttribute('data-slot') ?? el.getAttribute('data-sidebar') ?? el.getAttribute('role');
		const said = (el.getAttribute('aria-label') ?? el.textContent ?? '')
			.trim()
			.replace(/\s+/g, ' ')
			.slice(0, 28);
		return `${el.tagName.toLowerCase()}${kind ? `[${kind}]` : ''}${said ? ` "${said}"` : ''}`;
	};
	const ownText = (el: Element) =>
		Array.from(el.childNodes).filter(
			(n) => n.nodeType === Node.TEXT_NODE && (n.textContent ?? '').trim() !== ''
		);
	const textBox = (n: Node) => {
		const range = document.createRange();
		range.selectNodeContents(n);
		return range.getBoundingClientRect();
	};

	// centre and overlap: the parts of every row.
	const ROWS =
		'button, a[href], summary, [role=menuitem], [role=menuitemradio], [role=menuitemcheckbox], [role=option], [role=tab], [role=radio], [data-slot=badge]';
	let rows = 0;
	for (const row of all) {
		if (!shown(row)) continue;
		// A control is measured against its own middle too; any other box that centres its children
		// in a line only has them compared with each other (its padding need not be even).
		const control = row.matches(ROWS);
		const s = css(row);
		if (
			!s.display.includes('flex') ||
			s.flexDirection.startsWith('column') ||
			s.alignItems !== 'center'
		)
			continue;
		const boxes: { what: string; r: DOMRect }[] = [];
		for (const n of Array.from(row.childNodes)) {
			if (n.nodeType === Node.TEXT_NODE) {
				const said = (n.textContent ?? '').trim();
				const r = textBox(n);
				if (said && r.width > 0) boxes.push({ what: `the text "${said.slice(0, 20)}"`, r });
			} else if (n instanceof Element) {
				const p = css(n).position;
				if (p !== 'absolute' && p !== 'fixed' && shown(n))
					boxes.push({ what: name(n), r: rect(n) });
			}
		}
		if (!boxes.length) continue;
		// A row that wrapped has a middle per line (see the head of this file).
		if (boxes.some((a) => boxes.some((b) => a.r.top >= b.r.bottom - 1))) continue;
		rows++;
		const c = mid(rect(row));
		for (const b of boxes) {
			const d = mid(b.r) - c;
			if (control && Math.abs(d) > 1)
				found.add(`centre: ${b.what} sits ${px(d)}px off the middle of ${name(row)}`);
		}
		const mids = boxes.map((b) => mid(b.r));
		const spread = Math.max(...mids) - Math.min(...mids);
		if (spread > 1)
			found.add(`centre: the parts of ${name(row)} are ${px(spread)}px apart in height`);
		for (let i = 0; i < boxes.length; i++)
			for (let j = i + 1; j < boxes.length; j++) {
				const a = boxes[i].r;
				const b = boxes[j].r;
				const w = Math.min(a.right, b.right) - Math.max(a.left, b.left);
				const h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
				if (w > 1 && h > 1)
					found.add(
						`overlap: ${boxes[i].what} and ${boxes[j].what} cover each other by ${px(w)}px in ${name(row)}`
					);
			}
	}

	// sidebar: what stands beside a sidebar button is not its child, so it is measured against it here.
	let actions = 0;
	for (const item of all) {
		if (!item.matches('[data-slot=sidebar-menu-item]') || !shown(item)) continue;
		const button = item.querySelector('[data-sidebar=menu-button]');
		if (!button) continue;
		const c = mid(rect(button));
		const tall = button.getAttribute('data-size') === 'lg';
		const title = button.querySelector('[data-row-text]');
		// Two lines: the block of both is what the row centres (see the head of this file).
		const label = title && title.parentElement !== button && tall ? title.parentElement : title;
		const parts: [string, Element | null][] = [
			['the title', label],
			['the state dot', button.querySelector('[data-slot=state-dot]')],
			['the icon', button.querySelector(':scope > svg')],
			['the count', item.querySelector(':scope > [data-sidebar=menu-badge]')],
			['the ••• button', item.querySelector(':scope > [data-sidebar=menu-action]')],
			['the ••• icon', item.querySelector(':scope > [data-sidebar=menu-action] svg')]
		];
		for (const [what, el] of parts) {
			if (!el || !shown(el)) continue;
			if (what === 'the ••• button') actions++;
			const d = mid(rect(el)) - c;
			if (Math.abs(d) > 1)
				found.add(`sidebar: ${what} sits ${px(d)}px off the middle of ${name(button)}`);
		}
		for (const beside of ['menu-action', 'menu-badge']) {
			const el = item.querySelector(`:scope > [data-sidebar=${beside}]`);
			if (!el || !title || !shown(el) || !shown(title)) continue;
			const over = rect(title).right - rect(el).left;
			if (over > 0.5)
				found.add(`sidebar: the title of ${name(button)} runs ${px(over)}px under its ${beside}`);
		}
	}

	let marks = 0;
	// head: a sheet's head is aligned at the top, with a title line as tall as its buttons.
	for (const head of all) {
		if (!head.matches('[data-slot=sheet-head]') || !shown(head)) continue;
		const title = head.querySelector('h1, h2, h3, [data-slot$=title]');
		if (!title || !shown(title)) continue;
		const range = document.createRange();
		range.selectNodeContents(title);
		const line = Array.from(range.getClientRects()).find((r) => r.width > 0 && r.height > 0);
		if (!line) continue;
		for (const b of Array.from(head.querySelectorAll('button'))) {
			if (!shown(b)) continue;
			const d = mid(rect(b)) - mid(line);
			if (Math.abs(d) > 1)
				found.add(`head: ${name(b)} sits ${px(d)}px off the title line of ${name(title)}`);
		}
	}

	// item: an item of several lines is read from the top, and its mark stands on its title's line.
	for (const item of all) {
		if (!item.matches('[data-slot=item]') || !shown(item)) continue;
		const media = item.querySelector(':scope > [data-slot=item-media]');
		const title = item.querySelector('[data-slot=item-title]');
		if (!media || !title || !shown(media) || !shown(title)) continue;
		// The browser names the top `flex-start`, `start` or `flow-start`, by how it was written.
		const own = css(media).alignSelf;
		const top = (own === 'auto' ? css(item).alignItems : own).endsWith('start');
		// A mark in the middle of its item is a part of a centred row, measured above.
		if (!top) continue;
		const range = document.createRange();
		range.selectNodeContents(title);
		const line = Array.from(range.getClientRects()).find((r) => r.width > 0 && r.height > 0);
		if (!line) continue;
		marks++;
		const d = mid(rect(media)) - mid(line);
		if (Math.abs(d) > 1)
			found.add(`item: the mark of ${name(item)} sits ${px(d)}px off the line of its title`);
	}

	// uniform: one height and one corner radius for the controls of a kind.
	const kinds = new Map<string, { el: Element; h: number; w: number; radius: string }[]>();
	const lists: Element[] = [];
	const listOf = (el: Element, sel: string) => {
		const list = el.closest(sel) ?? el.parentElement!;
		if (!lists.includes(list)) lists.push(list);
		return lists.indexOf(list) + 1;
	};
	const square = new Set<string>();
	const joined = new Set<string>();
	for (const el of all) {
		if (!shown(el)) continue;
		let key: string | null = null;
		if (el.matches('[data-slot=toggle-group-item]')) {
			key = `segment of toggle group ${listOf(el, '[data-slot=toggle-group]')}`;
			joined.add(key);
		} else if (el.matches('[data-sidebar=menu-button]'))
			key = `sidebar button (${el.getAttribute('data-size') ?? 'default'})`;
		else if (el.matches('[role=tab]')) key = `tab of list ${listOf(el, '[role=tablist]')}`;
		else if (el.matches('button[data-variant][data-size], a[data-variant][data-size]')) {
			const size = el.getAttribute('data-size')!;
			key = `button (${size})`;
			if (size.startsWith('icon')) square.add(key);
		} else if (el.matches('input[data-slot=input]:not([type=file])')) key = 'input';
		else if (
			el.matches('[data-slot=input-group]') &&
			!el.querySelector('textarea, [data-align^=block]')
		)
			key = 'input group';
		else if (el.matches('select, [data-slot=select-trigger]')) key = 'select';
		else if (el.matches('[role=menuitem], [role=menuitemradio], [role=menuitemcheckbox]'))
			key = 'menu item';
		else if (el.matches('[role=option]')) key = `option of list ${listOf(el, '[role=listbox]')}`;
		else if (el.matches('[data-slot=badge]')) key = 'badge';
		else if (el.matches('[data-slot=switch]')) key = 'switch';
		else if (el.matches('[data-slot=checkbox]')) key = 'checkbox';
		else if (el.matches('[data-slot=kbd]')) key = 'key cap';
		if (!key) continue;
		const r = rect(el);
		const list = kinds.get(key) ?? [];
		list.push({ el, h: r.height, w: r.width, radius: css(el).borderRadius });
		kinds.set(key, list);
	}
	for (const [key, list] of kinds) {
		const sign = (m: { h: number; radius: string }) =>
			joined.has(key) ? px(m.h) : `${px(m.h)}px tall with ${m.radius} corners`;
		const count = new Map<string, number>();
		for (const m of list) count.set(sign(m), (count.get(sign(m)) ?? 0) + 1);
		const usual = [...count].sort((a, b) => b[1] - a[1])[0][0];
		for (const m of list) {
			if (sign(m) !== usual)
				found.add(`uniform: ${key}: ${name(m.el)} is ${sign(m)}, the others are ${usual}`);
			if (square.has(key) && Math.abs(m.w - m.h) > 0.5)
				found.add(`uniform: ${key}: ${name(m.el)} is ${px(m.w)}px wide and ${px(m.h)}px tall`);
		}
	}

	// clip, spill, cut, scroll, viewport, font.
	const SCROLLERS =
		'pre, table, [data-slot=code-block-content], [data-slot=code-block-code], [data-slot=table-container]';
	const hides = (o: string) => o === 'hidden' || o === 'clip';
	const clipperOf = (el: Element, axis: 'x' | 'y', self = false) => {
		for (let p = self ? el : el.parentElement; p; p = p.parentElement) {
			const o = axis === 'x' ? css(p).overflowX : css(p).overflowY;
			if (o !== 'visible') return p;
		}
		return null;
	};
	const clamps = (el: Element) => css(el).getPropertyValue('-webkit-line-clamp') !== 'none';
	/** How far `r` reaches outside `box` on an axis; 0 when it is inside. */
	const past = (r: DOMRect, box: DOMRect, axis: 'x' | 'y') =>
		axis === 'x'
			? Math.max(r.right - box.right, box.left - r.left, 0)
			: Math.max(r.bottom - box.bottom, box.top - r.top, 0);
	const cutBy = (el: Element, axis: 'x' | 'y') => {
		const p = clipperOf(el, axis);
		if (!p) return null;
		const s = css(p);
		if (!hides(axis === 'x' ? s.overflowX : s.overflowY)) return null;
		// What an ellipsis or a line clamp hides is judged as text, below.
		if (s.textOverflow === 'ellipsis' || clamps(p)) return null;
		const by = past(rect(el), rect(p), axis);
		return by > 1 ? { p, by } : null;
	};
	const height = window.innerHeight;
	let texts = 0;
	if (document.documentElement.scrollWidth > view + 1)
		found.add(
			`viewport: the page scrolls sideways by ${document.documentElement.scrollWidth - view}px`
		);
	for (const el of all) {
		if (!shown(el)) continue;
		const s = css(el);
		const r = rect(el);
		if (
			(s.overflowX === 'auto' || s.overflowX === 'scroll') &&
			el.scrollWidth > el.clientWidth + 1 &&
			!el.matches(SCROLLERS)
		)
			found.add(
				`scroll: ${name(el)} scrolls sideways by ${el.scrollWidth - el.clientWidth}px and is no code, diff or table`
			);
		for (const axis of ['x', 'y'] as const) {
			// The fill of a meter is slid out of its track to show its value: that is what the track hides.
			if (el.matches('[data-slot=progress-indicator]')) continue;
			const cut = cutBy(el, axis);
			// Only the outermost box that is cut: what is inside it is cut with it.
			if (cut && !(el.parentElement !== cut.p && cutBy(el.parentElement!, axis)))
				found.add(
					`cut: ${name(el)} reaches ${px(cut.by)}px past ${name(cut.p)}, which hides it (${axis})`
				);
		}
		if (!clipperOf(el, 'x') && (r.right > view + 1 || r.left < -1))
			found.add(
				`viewport: ${name(el)} reaches ${px(Math.max(r.right - view, -r.left))}px outside the page sideways`
			);
		if (!clipperOf(el, 'y') && (r.bottom > height + 1 || r.top < -1))
			found.add(
				`viewport: ${name(el)} reaches ${px(Math.max(r.bottom - height, -r.top))}px outside the page vertically`
			);

		const field = el.matches('input:not([type=file]), textarea, select');
		const own = ownText(el);
		if (!own.length && !field) continue;
		texts++;
		const family = s.fontFamily.split(',')[0].trim().replace(/["']/g, '');
		const set = el.closest(
			'pre, code, samp, .font-mono, .num, .font-sans, [data-slot=code-block-content]'
		);
		const want = set && !set.matches('.font-sans') ? 'Geist Mono Variable' : 'Geist Variable';
		if (family !== want) found.add(`font: ${name(el)} is set in ${family}, not ${want}`);

		for (const n of own) {
			const t = textBox(n);
			if (t.width === 0 || t.height === 0) continue;
			const said = (n.textContent ?? '').trim().replace(/\s+/g, ' ').slice(0, 28);
			const cx = clipperOf(el, 'x', true);
			if (cx && past(t, rect(cx), 'x') > 1) {
				// An ellipsis is drawn only for text laid out in the lines of the box that clips it.
				let inline = true;
				for (let p: Element | null = el; p && p !== cx; p = p.parentElement) {
					const d = css(p).display;
					if (!d.startsWith('inline') && d !== 'contents') inline = false;
				}
				if (hides(css(cx).overflowX) && (css(cx).textOverflow !== 'ellipsis' || !inline))
					found.add(`clip: the text "${said}" is cut by ${name(cx)} without an ellipsis`);
			} else {
				let box: Element = el;
				while (css(box).display.startsWith('inline') && box.parentElement) box = box.parentElement;
				const by = past(t, rect(box), 'x');
				if (by > 1) found.add(`spill: the text "${said}" reaches ${px(by)}px outside ${name(box)}`);
			}
			const cy = clipperOf(el, 'y', true);
			if (cy && hides(css(cy).overflowY) && !clamps(cy)) {
				const by = past(t, rect(cy), 'y');
				if (by > 1)
					found.add(`clip: the text "${said}" is cut ${px(by)}px at the edge of ${name(cy)}`);
			} else if (cy && clamps(cy)) {
				// The lines a clamp leaves out are still laid out below the last one it keeps: a box
				// with padding under its lines shows the top of the next one there.
				const edge = rect(cy).bottom - parseFloat(css(cy).borderBottomWidth);
				const inner = edge - parseFloat(css(cy).paddingBottom);
				const range = document.createRange();
				range.selectNodeContents(n);
				if (Array.from(range.getClientRects()).some((l) => l.top >= inner - 1 && l.top < edge - 1))
					found.add(`clip: a line of "${said}" past the clamp of ${name(cy)} shows in its padding`);
			}
		}
	}
	for (const font of ['Geist Variable', 'Geist Mono Variable'])
		if (!document.fonts.check(`14px "${font}"`)) found.add(`font: ${font} is not loaded`);

	return { rows, actions, marks, kinds: kinds.size, texts, findings: [...found].sort() };
}

/** Waits until nothing is moving: every transition and animation that ends has ended. */
async function settle(page: Page) {
	await page.evaluate(async () => {
		const frame = () => new Promise((r) => requestAnimationFrame(() => r(null)));
		await document.fonts.ready;
		for (let quiet = 0; quiet < 3;) {
			await frame();
			// A spinner and the pulse of a running dot never end; they are not waited for.
			const moving = document
				.getAnimations()
				.some(
					(a) =>
						a.playState === 'running' &&
						Number.isFinite(Number(a.effect?.getComputedTiming().endTime ?? Infinity))
				);
			quiet = moving ? 0 : quiet + 1;
		}
	});
}

/** Fails when a toast lies on the header, on the banner above it or on the composer's Send button. */
async function toastsClear(page: Page, state: string) {
	const seen = await page.evaluate(() => {
		const toasts = Array.from(document.querySelectorAll('[data-sonner-toast]'));
		const parts = Array.from(
			document.querySelectorAll(
				'[data-session-header], [data-full-access], [data-composer] button[type=submit]'
			)
		);
		const covered: string[] = [];
		for (const toast of toasts) {
			const a = toast.getBoundingClientRect();
			for (const part of parts) {
				const b = part.getBoundingClientRect();
				const x = Math.min(a.right, b.right) - Math.max(a.left, b.left);
				const y = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
				if (x > 0 && y > 0)
					covered.push(
						`${part.getAttribute('aria-label') ?? part.tagName.toLowerCase()}: ${Math.round(x)}x${Math.round(y)}px`
					);
			}
		}
		return { toasts: toasts.length, parts: parts.length, covered };
	});
	expect(seen.toasts, `${state}: toasts on screen`).toBeGreaterThan(0);
	expect(seen.parts, `${state}: the header and the Send button`).toBeGreaterThanOrEqual(2);
	expect(seen.covered.join('; '), `${state}: what the toasts cover`).toBe('');
}

/** Where a part of a sheet was before it was scrolled to, and where it is in view. */
interface Reached {
	/** How much of it the box that scrolls it (or the footer under that box) hid before. */
	hiddenBefore: number;
	top: number;
	bottom: number;
	/** The part of the screen the box that scrolls it shows. */
	clipTop: number;
	clipBottom: number;
	/** The top of the sheet's footer; null when it has none. */
	footerTop: number | null;
	viewport: number;
	/** What a pointer at its top, its middle and its bottom lands on, when that is not the part. */
	covered: string[];
}

/**
 * Scrolls `target` into view and fails unless all of it is then on screen, inside the box that
 * scrolls it, above the footer of its sheet, and under nothing else.
 */
async function reach(target: Locator, what: string): Promise<Reached> {
	const place = (el: Element) => {
		const r = el.getBoundingClientRect();
		let scroller = el.parentElement;
		while (scroller && !/auto|scroll/.test(getComputedStyle(scroller).overflowY))
			scroller = scroller.parentElement;
		const clip = (scroller ?? document.documentElement).getBoundingClientRect();
		const footer = el
			.closest('[role=dialog], [data-slot=card]')
			?.querySelector('[data-slot$=footer]')
			?.getBoundingClientRect();
		const edge = Math.min(clip.bottom, footer?.top ?? Infinity, window.innerHeight);
		const x = r.left + r.width / 2;
		const covered = [r.top + 1, r.top + r.height / 2, r.bottom - 1]
			.map((y) => document.elementFromPoint(x, y))
			.filter((hit) => !hit || !el.contains(hit))
			.map((hit) =>
				hit ? `${hit.tagName.toLowerCase()}[${hit.getAttribute('data-slot') ?? ''}]` : 'nothing'
			);
		const round = (n: number) => Math.round(n * 100) / 100;
		return {
			hidden: round(Math.max(r.bottom - edge, clip.top - r.top, 0)),
			top: round(r.top),
			bottom: round(r.bottom),
			clipTop: round(clip.top),
			clipBottom: round(clip.bottom),
			footerTop: footer ? round(footer.top) : null,
			viewport: window.innerHeight,
			covered
		};
	};
	const before = await target.evaluate(place);
	await target.scrollIntoViewIfNeeded();
	const { hidden, ...now } = await target.evaluate(place);
	const seen = { hiddenBefore: before.hidden, ...now };
	const said = `${what}: ${JSON.stringify(seen)}`;
	expect(hidden, `${said}: what is still hidden of it`).toBe(0);
	expect(seen.top, `${said}: its top`).toBeGreaterThanOrEqual(seen.clipTop - 0.5);
	expect(seen.bottom, `${said}: its bottom`).toBeLessThanOrEqual(seen.clipBottom + 0.5);
	expect(seen.bottom, `${said}: its bottom`).toBeLessThanOrEqual(seen.viewport + 0.5);
	if (seen.footerTop !== null)
		expect(seen.bottom, `${said}: its bottom and the footer`).toBeLessThanOrEqual(
			seen.footerTop + 0.5
		);
	expect(seen.covered, `${said}: what covers it`).toEqual([]);
	return seen;
}

const OPEN =
	'[role=dialog]:visible, [role=alertdialog]:visible, [role=menu]:visible, [data-slot=popover-content]:visible';

/** Closes every dialog, menu and popover that is open, the top one first. */
async function closeAll(page: Page) {
	const open = page.locator(OPEN);
	// Escape goes first to a tooltip when one is open, and focus that comes back to a button opens
	// its tooltip: one press may close nothing that is counted here. Escape is pressed only while
	// something is open, so it never reaches the page (where it would stop a running agent).
	await expect(async () => {
		if (await open.count()) await page.keyboard.press('Escape');
		expect(await open.count()).toBe(0);
	}).toPass({ intervals: [100, 200, 400], timeout: 15_000 });
}

const PANES = [
	['general', 'General'],
	['models', 'Models and roles'],
	['permissions', 'Permissions'],
	['usage', 'Usage'],
	['context', 'Context'],
	['all', 'All settings'],
	['devices', 'Devices and remote'],
	['shortcuts', 'Keyboard shortcuts'],
	['about', 'About']
] as const;

const LONG = 'a-service-with-a-long-name-that-cannot-fit-in-the-sidebar';
// A one-pixel PNG, to attach.
const PIXEL = Buffer.from(
	'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
	'base64'
);

/**
 * Profiles for the scripted provider: `mine` keeps the lead on its model, `fallback` is where a
 * usage limit sends it. The built-in ones are listed too, and need logins this home does not have.
 */
const PROFILES = [
	'[profiles]',
	'on_limit = "fallback"',
	'',
	'[profiles.mine.roles]',
	'default = ["mock/lead"]',
	'smol = ["mock/scout"]',
	'',
	'[profiles.fallback.roles]',
	'default = ["mock/scout"]',
	''
].join('\n');
/** A session whose second request runs out of allowance, and is answered by the fallback. */
const LIMITED = {
	lead: [{ reply: { text: 'Ready.' } }, { usage_limit: { resets_in_secs: 8040 } }],
	scout: [{ reply: { text: 'On the fallback.' } }]
};

test.describe.configure({ timeout: 300_000 });

for (const view of VIEWS)
	for (const theme of THEMES)
		at(view, `layout holds at ${view.name}, ${theme}`, async ({ page }) => {
			const { width, height } = view;
			const narrow = width < 768;
			const dir = join(SHOTS, `${view.name}-${theme}`);
			mkdirSync(dir, { recursive: true });
			const measured: Record<string, Measured> = {};
			const reached: Record<string, Reached> = {};
			const where = new Map<string, string[]>();
			let shot = 0;

			/** Measures the page as it stands and saves what it looks like. */
			const capture = async (state: string) => {
				await settle(page);
				const m = await page.evaluate(audit, 'body');
				const file = `${String(++shot).padStart(2, '0')}-${state}`;
				await page.screenshot({ path: join(dir, `${file}.png`) });
				measured[file] = m;
				for (const f of m.findings) where.set(f, [...(where.get(f) ?? []), state]);
				// A state with nothing measured in it would pass whatever it looked like.
				expect(m.rows, `${state}: rows measured`).toBeGreaterThan(0);
				expect(m.texts, `${state}: texts measured`).toBeGreaterThan(0);
				return m;
			};

			const stack = new Stack();
			stack.config = PROFILES;
			try {
				const work = join(stack.dir, 'work');
				await stack.start(
					'auto',
					false,
					['--set', `remote.folders=['${stack.project}','${work}']`],
					MAIN
				);
				const main = stack.sessionId;
				const earlier = stack.session('an earlier session', DONE);
				await stack.until({ [earlier]: 'idle' });
				stack.end(earlier);
				const short = stack.session('say done', DONE, 'auto', join(work, 'api'));
				await stack.until({ [short]: 'idle' });
				const long = stack.session('say done', DONE, 'auto', join(work, LONG));
				await stack.until({ [long]: 'idle' });
				const asks = stack.session('create hello', MAIN, 'manual', join(work, 'approvals'));
				await stack.until({ [asks]: 'waiting for you' });
				stack.mockTools = true;
				const busy = stack.session('wait for it', BUSY, 'auto', join(work, 'api'));
				await stack.until({ [busy]: 'working' });
				const team = stack.session('split the work', TEAM, 'auto', join(work, 'team'));
				await stack.until({ [team]: 'working' });

				await page.setViewportSize({ width, height });
				await page.addInitScript((t) => localStorage.setItem('azazel-theme', t), theme);

				const list = page.locator('[data-session-list]:visible').first();
				const drawer = page.locator('[data-slot=sidebar][data-mobile=true]');
				/** The sidebar on screen: on a phone it is a sheet that has to be opened. */
				const sidebar = async () => {
					if (narrow && !(await drawer.isVisible())) {
						await page.getByRole('button', { name: 'Open the sidebar' }).click();
						await expect(drawer).toBeVisible();
					}
					return narrow ? drawer : page.locator('[data-slot=sidebar][data-state]');
				};
				const rowOf = (id: string) =>
					page
						.locator('[data-session-list]:visible [data-session-row]', {
							has: page.locator(`a[href="/s/${id}"]`)
						})
						.first();
				const open = async (id: string) => {
					await sidebar();
					await rowOf(id).getByRole('link').click();
					await page.waitForURL(new RegExp(`/s/${id}$`));
					if (narrow) await expect(drawer).toHaveCount(0);
					// The pointer rests where no tooltip opens.
					await page.mouse.move(width - 2, height - 2);
				};
				const box = page.getByRole('combobox', { name: 'Message' });

				// The sessions, before one is open.
				await page.goto(stack.pair);
				await expect(list.getByRole('link')).toHaveCount(6);
				await expect(page.locator('html')).toHaveClass(new RegExp(`\\b${theme}\\b`));
				await capture('home');

				// The session: markdown, a code block, a table, tool cards (one of them failed).
				if (narrow) {
					await rowOf(main).getByRole('link').click();
					await page.waitForURL(new RegExp(`/s/${main}$`));
				} else await open(main);
				await expect(page.getByRole('heading', { name: 'All done' })).toBeVisible();
				await expect(page.locator('[data-transcript] pre').first()).toBeVisible();
				await expect(page.getByRole('paragraph').filter({ hasText: UNVERIFIED })).toBeVisible();
				await expect(box).toBeVisible();
				await page.mouse.move(width - 2, height - 2);
				await capture('session');

				// The sidebar: long and short titles, folder lines, a row hovered, its menu, renaming.
				await sidebar();
				const row = rowOf(long);
				const action = row.locator('[data-sidebar=menu-action]');
				await row.getByRole('link').hover();
				await expect(action).toHaveCSS('opacity', '1');
				await expect(page.locator('[data-slot=tooltip-content]')).toBeVisible();
				const hovered = await capture('row-hover');
				expect(hovered.actions, 'the ••• buttons measured').toBeGreaterThanOrEqual(5);
				// The long title is there in full for a reader of the page, and ends in an ellipsis.
				const title = row.locator('[data-row-text]');
				await expect(title).toHaveText(LONG);
				await expect(title).toHaveCSS('text-overflow', 'ellipsis');
				await action.click();
				await expect(page.getByRole('menu')).toBeVisible();
				await expect(page.locator('[data-slot=tooltip-content]')).toHaveCount(0);
				await capture('row-menu');
				await page.getByRole('menuitem', { name: 'Rename' }).click();
				const rename = page.getByRole('dialog', { name: 'Rename session' });
				await expect(rename.getByLabel('Name')).toHaveValue(LONG);
				await capture('rename');
				await rename.getByRole('button', { name: 'Cancel' }).click();
				await expect(rename).toHaveCount(0);
				await closeAll(page);

				// Tool calls open in place, on every screen, into a card per call. Each run is opened,
				// brought to the top of the transcript, measured, and closed again.
				const run = (n: number) => page.locator('[data-transcript] [data-steps]').nth(n);
				const cards = (n: number) => run(n).locator('[data-tool-cards]');
				const toggle = async (n: number, open: boolean) => {
					const row = run(n).locator('> button');
					await row.click();
					await expect(row).toHaveAttribute('aria-expanded', String(open));
					if (open) {
						await expect(cards(n)).toBeVisible();
						await run(n).evaluate((el) => el.scrollIntoView({ block: 'start' }));
					} else await expect(cards(n)).toBeHidden();
					await page.mouse.move(width - 2, height - 2);
				};
				/** How many lines of its own a scrolling block shows, and whether it has more than that. */
				const block = (el: HTMLElement) => {
					const s = getComputedStyle(el);
					const padding = parseFloat(s.paddingTop) + parseFloat(s.paddingBottom);
					return {
						lines: (el.clientHeight - padding) / parseFloat(s.lineHeight),
						scrolls: el.scrollHeight > el.clientHeight + 1
					};
				};
				await expect(page.locator('[data-transcript] [data-steps]')).toHaveCount(3);

				// A file that was written: its path, what it added, and its lines with their numbers.
				await toggle(0, true);
				const write = cards(0).locator('[data-tool="write"]');
				await expect(write).toHaveAttribute('data-status', 'done');
				await expect(write.getByRole('heading', { name: 'src/Welcome.svelte' })).toBeVisible();
				await expect(write.locator('[data-tool-meta]')).toContainText('+31');
				const added = write.locator('[data-line=add]');
				await expect(added).toHaveCount(31);
				await expect(added.first().locator('[data-line-number]')).toHaveText('1');
				await expect(added.last().locator('[data-line-number]')).toHaveText('31');
				// The bar of an added line is the green of success, on every line.
				expect(
					await added.first().evaluate((el) => {
						const probe = document.createElement('i');
						probe.style.color = 'var(--success)';
						el.append(probe);
						const want = getComputedStyle(probe).color;
						probe.remove();
						const s = getComputedStyle(el);
						return s.borderLeftColor === want && parseFloat(s.borderLeftWidth) === 2;
					}),
					'the green bar of an added line'
				).toBe(true);
				const file = await write.locator('[data-diff-lines]').evaluate(block);
				expect(file.scrolls, 'a long file scrolls inside its card').toBe(true);
				expect(file.lines, 'lines of a file shown at once').toBeLessThanOrEqual(20.5);
				await expect(cards(0).locator('[data-tool="read"]')).toHaveAttribute('data-status', 'done');
				await expect(cards(0).locator('[data-tool="todo"] [data-tool-command]')).toBeVisible();
				await capture('tool-file');
				await toggle(0, false);

				// Two commands: one with a line of output, one with more than its card shows at once.
				await toggle(1, true);
				const shells = cards(1).locator('[data-tool="shell"]');
				await expect(shells).toHaveCount(2);
				for (const n of [0, 1]) await expect(shells.nth(n)).toHaveAttribute('data-status', 'done');
				await expect(shells.first().getByRole('heading', { name: 'Bash' })).toBeVisible();
				await expect(shells.first().locator('[data-tool-command]')).toHaveText(
					'$echo "hello from the shell"'
				);
				await expect(shells.first().getByLabel('Output', { exact: true })).toContainText(
					'hello from the shell'
				);
				// The command is coloured once its language is loaded.
				await expect(
					shells.first().locator('[data-tool-command] [class^=hljs-]').first()
				).toBeVisible();
				const output = shells.nth(1).getByLabel('Output', { exact: true });
				await expect(output).toContainText('line 60 of the output');
				const tall = await output.evaluate(block);
				expect(tall.scrolls, 'long output scrolls inside its card').toBe(true);
				expect(tall.lines, 'lines of output shown at once').toBeLessThanOrEqual(20.5);
				expect(tall.lines, 'lines of output shown at once').toBeGreaterThan(15);
				await capture('tool-commands');
				// The output brings its own copy button when the pointer is on it.
				const copyOut = shells.nth(1).getByRole('button', { name: 'Copy the output' });
				await shells.nth(1).evaluate((el) => el.scrollIntoView({ block: 'start' }));
				await output.hover();
				await expect(copyOut).toHaveCSS('opacity', '1');
				await capture('tool-output');
				await toggle(1, false);

				// A call that failed says so on its row, and again on its card.
				await expect(run(2)).toHaveAttribute('data-status', 'error');
				await expect(run(2).locator('> button [data-slot=badge]')).toHaveText('Failed');
				await toggle(2, true);
				const failed = cards(2).locator('[data-tool="read"]');
				await expect(failed).toHaveAttribute('data-status', 'error');
				await expect(failed.locator('[data-slot=badge]')).toHaveText('Failed');
				await expect(failed.getByLabel('Error', { exact: true })).toBeVisible();
				await capture('tool-failed');
				await toggle(2, false);

				/** Whether what `sel` is painted with (`prop`) is the accent as it stands. */
				const inAccent = (sel: string, prop: 'color' | 'backgroundColor') =>
					page.evaluate(
						([sel, prop]) => {
							const el = Array.from(document.querySelectorAll<HTMLElement>(sel)).find((e) =>
								e.checkVisibility()
							);
							if (!el) return `${sel} is not on screen`;
							const probe = document.createElement('i');
							probe.style.color = 'var(--brand)';
							document.body.append(probe);
							const want = getComputedStyle(probe).color;
							probe.remove();
							const got = getComputedStyle(el)[prop as 'color'];
							return got === want ? '' : `${sel} is ${got}, the accent is ${want}`;
						},
						[sel, prop]
					);

				// The right pane (a sheet on a phone): usage, with its meter and the plan in the accent.
				if (width >= 1024) {
					const pane = page.locator('[data-right-pane]');
					const paneWidth = () => pane.evaluate((el) => el.getBoundingClientRect().width);
					const moved = page.locator('[data-right-pane][data-sliding]');
					await expect(pane).toHaveAttribute('data-right-pane', 'open');
					const wide = await paneWidth();
					expect(wide, 'the width of the open pane').toBeGreaterThan(200);
					await pane.getByRole('tab', { name: 'Usage' }).click();
					await expect(pane.locator('[data-usage]')).toContainText('Check it from the shell');
					await capture('right-usage');
					expect(
						await inAccent('[data-usage] [data-slot=progress-indicator]', 'backgroundColor')
					).toBe('');
					expect(await inAccent('[data-usage] li svg.text-brand', 'color')).toBe('');

					// Closed from the top bar, its width runs to nothing; it is not gone at once.
					const widths = await page.evaluate(async () => {
						const el = document.querySelector('[data-right-pane]')!;
						document
							.querySelector<HTMLElement>('button[aria-label="Hide the side panel"]')!
							.click();
						const seen: number[] = [];
						const start = performance.now();
						while (performance.now() - start < 400) {
							await new Promise((r) => requestAnimationFrame(r));
							seen.push(el.getBoundingClientRect().width);
						}
						return seen;
					});
					expect(
						widths.some((w) => w > 1 && w < wide - 1),
						`the pane is seen on its way: ${widths.map(Math.round).join(' ')}`
					).toBe(true);
					await expect(pane).toHaveAttribute('data-right-pane', 'closed');
					await expect(moved).toHaveCount(0);
					expect(await paneWidth(), 'the width of the closed pane').toBeLessThan(0.5);
					await expect(
						page.getByRole('complementary', { name: 'Agents, usage and browser' })
					).toHaveCount(0);
					await page.mouse.move(width - 2, height - 2);
					await capture('right-closed');
					await page.getByRole('button', { name: 'Show the side panel' }).click();
					await expect(pane).toHaveAttribute('data-right-pane', 'open');
					await expect(moved).toHaveCount(0);
					await settle(page);
					// It comes back as wide as it was, with the handle that resizes it.
					expect(
						Math.abs((await paneWidth()) - wide),
						'the pane is as wide as before'
					).toBeLessThan(1);
					await expect(
						page.getByRole('separator', { name: 'Resize the side panel' })
					).toBeVisible();
					await pane.getByRole('tab', { name: 'Agents' }).click();
					await page.mouse.move(width - 2, height - 2);
					await capture('right-open');
				} else {
					await page.getByRole('button', { name: 'Agents, usage and browser' }).click();
					const sheet = page.getByRole('dialog', { name: 'Agents' });
					await sheet.getByRole('radio', { name: 'Usage' }).click();
					await expect(page.locator('[data-usage]')).toContainText('Check it from the shell');
					await capture('right-usage');
					expect(
						await inAccent('[data-usage] [data-slot=progress-indicator]', 'backgroundColor')
					).toBe('');
					await closeAll(page);
				}

				// The pickers of the status bar.
				for (const which of ['Model', 'Profile', 'Thinking', 'Mode', 'Agent']) {
					const chip = page
						.locator('[data-statusbar]')
						.getByRole('button', { name: new RegExp(`^${which}:`) });
					// The agent chip is there only when the session names its agent.
					if (which === 'Agent' && !(await chip.count())) continue;
					await chip.click();
					await expect(page.locator(OPEN).first()).toBeVisible();
					if (which === 'Model') await expect(page.getByRole('option').first()).toBeVisible();
					await page.mouse.move(width - 2, 2);
					await capture(`picker-${which.toLowerCase()}`);
					await closeAll(page);
				}

				// The composer: the `/` list, the `@` list, an attached image, a toast.
				await box.fill('/');
				await expect(page.getByRole('listbox', { name: 'Commands' })).toBeVisible();
				await capture('slash');
				await box.fill('@');
				await expect(page.getByRole('listbox', { name: 'Mentions' })).toBeVisible();
				await capture('mention');
				await box.fill('');
				await page
					.locator('input[type=file][accept="image/*"]')
					.setInputFiles({ name: 'pixel.png', mimeType: 'image/png', buffer: PIXEL });
				await expect(page.getByRole('img', { name: 'Attached 1' })).toBeVisible();
				await capture('image');
				await page.getByRole('button', { name: 'Remove image 1' }).click();
				await box.fill('/trust');
				await box.press('Enter');
				const toast = page.locator('[data-sonner-toast]');
				await expect(toast).toBeVisible();
				await capture('toast');
				await toastsClear(page, 'toast');
				// It leaves by itself; nothing after this is measured while it does.
				await expect(toast).toHaveCount(0, { timeout: 20_000 });
				await box.fill('');
				// An error, in one line, and the button that closes it.
				const closeToast = page.getByRole('button', { name: 'Close toast' });
				await box.fill('/export pdf');
				await box.press('Enter');
				await expect(toast).toHaveText(/Export as markdown, html or jsonl\./);
				await expect(toast).toHaveAttribute('data-type', 'error');
				await page.mouse.move(width - 2, 2);
				await capture('toast-error');
				await toastsClear(page, 'toast-error');
				// A warning that comes while the error is still there: the two, one above the other.
				await box.fill('/name ');
				await box.press('Enter');
				await expect(toast).toHaveCount(2);
				await expect(toast.first()).toHaveAttribute('data-type', 'warning');
				await expect(toast.first()).toHaveText(/\/name needs <title>\./);
				await toast.first().hover();
				await expect(toast.last()).toHaveAttribute('data-expanded', 'true');
				await capture('toast-stack');
				await toastsClear(page, 'toast-stack');
				await closeToast.last().click();
				await expect(toast).toHaveCount(1);
				await page.mouse.move(width - 2, 2);
				await capture('toast-warning');
				await toastsClear(page, 'toast-warning');
				await closeToast.click();
				await expect(toast).toHaveCount(0);
				await box.fill('');

				// Search, a new chat, the resume picker.
				await page.keyboard.press('Control+k');
				await expect(page.getByRole('listbox', { name: 'Results' })).toBeVisible();
				await capture('search');
				await closeAll(page);
				await (await sidebar()).getByRole('button', { name: 'New chat', exact: true }).click();
				const chat = page.getByRole('dialog', { name: 'New chat' });
				await expect(
					chat.getByRole('list', { name: 'Folders' }).getByRole('button').first()
				).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('new-chat');
				await closeAll(page);
				await (await sidebar()).getByRole('button', { name: 'Resume a session' }).click();
				const resume = page.getByRole('dialog', { name: 'Resume a session' });
				await expect(
					resume.getByRole('list', { name: 'Earlier sessions' }).getByRole('button').first()
				).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('resume');
				await closeAll(page);

				// The settings window, pane by pane, and the pairing sheet opened from it.
				await page.keyboard.press('Control+.');
				const win = page.locator('[role=dialog][data-settings]');
				await expect(win).toBeVisible();
				if (narrow) await capture('settings');
				for (const [id, label] of PANES) {
					await win.getByRole('tab', { name: label, exact: true }).click();
					await expect(win.locator(`[data-pane="${id}"]`)).toBeVisible();
					await expect(win.locator('[aria-busy=true]')).toHaveCount(0);
					await page.mouse.move(width - 2, 2);
					await capture(`settings-${id}`);
					if (id === 'general') {
						// The theme cards: each draws the page small in its own colours, and all are one height.
						const themes = await win.locator('[data-theme-card]').evaluateAll((els) =>
							els.map((el) => {
								const picture = el.querySelector<HTMLElement>('[data-theme-preview]')!;
								return {
									name: el.getAttribute('data-theme-card'),
									height: el.getBoundingClientRect().height,
									picture: picture.getBoundingClientRect().height,
									ground: getComputedStyle(picture).backgroundColor
								};
							})
						);
						expect(themes.map((t) => t.name)).toEqual(['terminal', 'dark', 'light']);
						for (const t of themes) {
							expect(
								Math.abs(t.height - themes[0].height),
								`the height of the ${t.name} card`
							).toBeLessThan(0.5);
							expect(t.picture, `the picture of the ${t.name} card`).toBe(themes[0].picture);
						}
						expect(themes[1].ground, 'the dark and the light picture differ').not.toBe(
							themes[2].ground
						);
						// The accents: each is chosen, drawn in every place the accent is, and measured.
						const accents = win.locator('[data-accents]');
						await accents.scrollIntoViewIfNeeded();
						await expect(accents.getByRole('radio')).toHaveCount(ACCENTS.length);
						for (const [value, label] of ACCENTS) {
							const swatch = accents.getByRole('radio', { name: label });
							await swatch.click();
							await expect(swatch).toHaveAttribute('aria-checked', 'true');
							await page.mouse.move(width - 2, 2);
							expect(await inAccent(`[data-swatch="${value}"]`, 'backgroundColor')).toBe('');
							// The picture of the theme in use shows the accent that was just chosen.
							expect(
								await inAccent(
									`[data-theme-card=${theme}] [data-preview-accent]`,
									'backgroundColor'
								),
								`${value} in the picture of the ${theme} theme`
							).toBe('');
							await capture(`accent-${value}`);
						}
						await accents.getByRole('radio', { name: 'Emerald' }).click();
						await win.locator('[data-pane=general]').evaluate((el) => {
							for (let p: Element | null = el; p; p = p.parentElement) p.scrollTop = 0;
						});
					}
					if (id === 'devices') {
						await win.getByRole('button', { name: 'New link' }).click();
						const pairing = page.getByRole('dialog', { name: 'Open on another device' });
						await expect(pairing.getByRole('button', { name: 'Make a link' })).toBeVisible();
						await page.mouse.move(width - 2, 2);
						await capture('pairing');
						await pairing.getByRole('button', { name: 'Make a link' }).click();
						await expect(pairing.locator('[data-qr]')).toBeVisible();
						await page.mouse.move(width - 2, 2);
						await capture('pairing-link');
						await page.keyboard.press('Escape');
						await expect(pairing).toHaveCount(0);
					}
					if (narrow) await win.getByRole('button', { name: 'All settings panes' }).click();
				}
				await closeAll(page);

				// The sidebar folded to a rail of icons (a phone has a drawer in its place), and the
				// tooltip that names an icon.
				if (!narrow) {
					const rail = page.locator('[data-slot=sidebar][data-collapsible=icon]');
					await page.getByRole('button', { name: 'Hide the sidebar' }).click();
					await expect(rail).toBeVisible();
					await expect(page.locator('[data-session-list]:visible')).toHaveCount(0);
					await page.mouse.move(width - 2, height - 2);
					const folded = await capture('sidebar-icons');
					expect(folded.actions, 'no ••• button on the rail').toBe(0);
					// Every icon of the rail is in the middle of it, sideways too.
					expect(
						await rail.evaluate((el) => {
							const inner = el.querySelector('[data-slot=sidebar-inner]')!.getBoundingClientRect();
							const middle = inner.left + inner.width / 2;
							return Array.from(el.querySelectorAll('button:not([data-sidebar=rail])'))
								.filter((b) => b.checkVisibility())
								.map((b) => {
									const r = b.querySelector('svg')!.getBoundingClientRect();
									return `${b.getAttribute('aria-label')}: ${Math.round((r.left + r.width / 2 - middle) * 100) / 100}`;
								})
								.filter((s) => Math.abs(Number(s.split(': ')[1])) > 1);
						}),
						'icons off the middle of the rail'
					).toEqual([]);
					await rail.getByRole('button', { name: /^Search sessions/ }).hover();
					await expect(page.locator('[data-slot=tooltip-content]')).toBeVisible();
					await capture('sidebar-icons-tip');
					await page.mouse.move(width - 2, height - 2);
					await page.getByRole('button', { name: 'Show the sidebar' }).click();
					await expect(rail).toHaveCount(0);
					await expect(page.locator('[data-session-list]:visible')).toHaveCount(1);
					await page.mouse.move(width - 2, height - 2);
				}

				// Going back: two more prompts, the sheet that lists them (one too long for its row), and
				// the choice of what to restore, with the prompt quoted above it.
				for (const [prompt, answer] of [
					[LONG_PROMPT, 'Noted.'],
					['And thanks.', 'Noted, again.']
				]) {
					await box.fill(prompt);
					await box.press('Enter');
					await expect(
						page.getByRole('paragraph').filter({ hasText: new RegExp(`^${answer}$`) })
					).toBeVisible();
				}
				await page.getByRole('button', { name: 'Session actions' }).click();
				await page.getByRole('menuitem', { name: 'Rewind…' }).click();
				const rewind = page.getByRole('dialog', { name: 'Rewind' });
				const prompts = rewind.getByRole('list', { name: 'Prompts' }).getByRole('button');
				// Newest first: the two from the page and the first prompt. The note of the done check is a
				// user message of the conversation too, and is not one of them.
				await expect(prompts).toHaveCount(3);
				await expect(prompts.filter({ hasText: 'done-check' })).toHaveCount(0);
				await expect(prompts.first().locator('[data-slot=item-title]')).toHaveText('And thanks.');
				const wordy = prompts.filter({ hasText: 'Now the same for the settings' });
				await expect(wordy.locator('[data-slot=item-title]')).toHaveCSS(
					'text-overflow',
					'ellipsis'
				);
				await page.mouse.move(width - 2, 2);
				await capture('rewind');
				await wordy.click();
				await expect(rewind.getByRole('radiogroup', { name: 'What to restore' })).toBeVisible();
				await expect(rewind.getByRole('radio')).toHaveCount(3);
				await expect(rewind.locator('blockquote')).toContainText('Now the same for the settings');
				await expect(rewind.getByRole('button', { name: 'Back' })).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('rewind-choose');
				// The last choice, which a short window starts below the fold: all of its card can be
				// brought above the footer, and it is chosen with a click.
				const files = rewind.getByRole('radio', { name: /^Files only/ });
				reached['rewind-files-only'] = await reach(
					rewind.locator('[data-slot=field-label]').filter({ hasText: 'Files only' }),
					'the choice Files only'
				);
				await files.click();
				await expect(files).toHaveAttribute('aria-checked', 'true');
				await page.mouse.move(width - 2, 2);
				await capture('rewind-files-only');
				await rewind.getByRole('button', { name: 'Cancel' }).click();
				await expect(rewind).toHaveCount(0);
				await closeAll(page);

				// A session that asks: the approval, docked beside the chat or as a sheet on a phone.
				await open(asks);
				await expect(page.locator('[data-request="approval"]')).toBeVisible();
				await capture('approval');

				// A session at work: a running tool card and the button that stops it, at rest and held.
				if (narrow) await page.goto(`http://127.0.0.1:${stack.port}/s/${busy}`);
				else await open(busy);
				const stop = page.getByRole('button', { name: 'Stop the agent' });
				await expect(stop).toBeVisible();
				await expect(page.locator('[data-steps]').first()).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('running');
				// Held, the wash is on its way up: that is looked at, not measured. Leaving the button
				// before the hold is over stops nothing.
				await stop.hover();
				await page.mouse.down();
				await expect(stop).toHaveAttribute('data-holding', '');
				await page.screenshot({
					path: join(dir, `${String(++shot).padStart(2, '0')}-holding.png`)
				});
				await page.mouse.move(width - 2, 2);
				await page.mouse.up();

				// A lead with a team: three agents at work (one of them started by another, one with a
				// name as long as a name may be) and one that is done, in the pane beside the chat or in
				// its sheet; the question before one is stopped; and the page of one of them.
				if (narrow) await page.goto(`http://127.0.0.1:${stack.port}/s/${team}`);
				else await open(team);
				await expect(box).toBeVisible();
				const hub = page.locator('[data-agent-hub]:visible');
				if (width >= 1024) {
					await expect(page.locator('[data-right-pane]')).toHaveAttribute(
						'data-right-pane',
						'open'
					);
					await page.locator('[data-right-pane]').getByRole('tab', { name: 'Agents' }).click();
				} else await page.getByRole('button', { name: 'Agents, usage and browser' }).click();
				const member = (id: string) => hub.locator(`[data-agent="${id}"]`);
				await expect(hub.locator('[data-agent]')).toHaveCount(5);
				for (const id of [ROUTES, LONG_AGENT, CALLERS])
					await expect(member(id).locator('[data-slot=state-dot]')).toHaveAttribute(
						'data-state',
						'working'
					);
				await expect(member('docs').locator('[data-slot=state-dot]')).toHaveAttribute(
					'data-state',
					'done'
				);
				await expect(member(LONG_AGENT)).toContainText('test_wait cargo test');
				await expect(hub.getByRole('button', { name: /^Stop / })).toHaveCount(3);
				// The lead waits for its team, so it is at work too.
				await expect(hub.locator('[data-slot=badge]')).toHaveText('4 active');
				await page.mouse.move(width - 2, 2);
				const agents = await capture('agents');
				expect(agents.marks, 'the state dots measured against their names').toBe(5);
				// The Stop buttons are one column, whatever the depth of their agent.
				const stops = await hub
					.getByRole('button', { name: /^Stop / })
					.evaluateAll((els) => els.map((el) => el.getBoundingClientRect().right));
				for (const right of stops)
					expect(Math.abs(right - stops[0]), 'the Stop buttons end on one edge').toBeLessThan(0.5);
				// The last agent, which a short window starts below the fold: its whole row can be
				// brought into view, and its link takes a click there.
				const last = hub.locator('[data-agent]').last();
				reached['agents-last'] = await reach(last, 'the last agent');
				await last.getByRole('link').click({ trial: true });
				await page.mouse.move(width - 2, 2);
				await capture('agents-last');
				await hub.getByRole('button', { name: `Stop ${CALLERS}` }).click();
				const sure = page.getByRole('alertdialog', { name: `Stop ${CALLERS}?` });
				await expect(sure.getByRole('button', { name: 'Keep' })).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('agent-stop');
				await sure.getByRole('button', { name: 'Keep' }).click();
				await expect(sure).toHaveCount(0);
				await member(LONG_AGENT).getByRole('link').click();
				await page.waitForURL(new RegExp(`/agents/${LONG_AGENT}$`));
				await closeAll(page);
				const bar = page.locator('[data-agent-bar]');
				await expect(bar).toContainText(LONG_AGENT);
				await expect(page.getByLabel(`Message ${LONG_AGENT}`)).toBeVisible();
				await expect(page.locator('[data-transcript]')).toContainText(
					'Running the tests of the store.'
				);
				await page.mouse.move(width - 2, 2);
				await capture('agent-page');
			} finally {
				writeFileSync(join(dir, 'measured.json'), JSON.stringify(measured, null, '\t'));
				writeFileSync(join(dir, 'reach.json'), JSON.stringify(reached, null, '\t'));
				await stack.stop();
			}

			const report = [...where].map(([f, states]) => `${f}  [${states.join(', ')}]`).sort();
			expect(report.join('\n'), `${basename(dir)}: ${report.length} findings`).toBe('');
		});

// Profiles, on a device with full access (which may save one): the chip before and after a profile
// is chosen, its menu, the Profiles section of the settings, and the notice of a usage limit.
for (const view of VIEWS)
	for (const theme of THEMES)
		at(view, `profiles hold at ${view.name}, ${theme}`, async ({ page }) => {
			const { width, height } = view;
			const dir = join(SHOTS, `${view.name}-${theme}-profiles`);
			mkdirSync(dir, { recursive: true });
			const measured: Record<string, Measured> = {};
			const where = new Map<string, string[]>();
			let shot = 0;
			const capture = async (state: string) => {
				await settle(page);
				const m = await page.evaluate(audit, 'body');
				const file = `${String(++shot).padStart(2, '0')}-${state}`;
				await page.screenshot({ path: join(dir, `${file}.png`) });
				measured[file] = m;
				for (const f of m.findings) where.set(f, [...(where.get(f) ?? []), state]);
				expect(m.rows, `${state}: rows measured`).toBeGreaterThan(0);
				expect(m.texts, `${state}: texts measured`).toBeGreaterThan(0);
				return m;
			};

			const stack = new Stack();
			stack.config = `${PROFILES}\n[remote]\nfull_access = true\n`;
			try {
				await stack.start('auto', false, [], LIMITED);
				await page.setViewportSize({ width, height });
				await page.addInitScript((t) => localStorage.setItem('azazel-theme', t), theme);
				await page.goto(stack.pair);
				await page.locator('[data-session-list]:visible a').first().click();
				await page.waitForURL(/\/s\/[^/]+$/);
				await expect(page.locator('html')).toHaveClass(new RegExp(`\\b${theme}\\b`));
				await expect(page.getByRole('paragraph').filter({ hasText: 'Ready.' })).toBeVisible();
				const box = page.getByRole('combobox', { name: 'Message' });
				await expect(box).toBeVisible();

				// The chip, beside the model's: the session is on no profile yet.
				const chip = page.locator('[data-statusbar]').getByRole('button', { name: /^Profile:/ });
				await expect(chip).toHaveAccessibleName('Profile: none');
				await page.mouse.move(width - 2, 2);
				await capture('chip');

				// Its menu: the built-in profiles need a login and cannot be chosen, the two of the
				// configuration can. Every row is one height.
				await chip.click();
				const menu = page.getByRole('menu');
				const item = (name: string) => menu.locator(`[role=menuitemradio][data-profile="${name}"]`);
				await expect(item('mine')).toBeVisible();
				await expect(item('mine')).toContainText('your config');
				await expect(item('mine')).toContainText('lead · scout');
				await expect(item('gpt')).toHaveAttribute('aria-disabled', 'true');
				await expect(item('gpt')).toContainText('needs azazel login openai-codex');
				await page.mouse.move(width - 2, 2);
				await capture('menu');
				await item('mine').click();
				await expect(menu).toHaveCount(0);
				await expect(chip).toHaveAccessibleName('Profile: mine');
				await chip.click();
				await expect(item('mine')).toHaveAttribute('aria-checked', 'true');
				await page.mouse.move(width - 2, 2);
				await capture('menu-checked');
				await closeAll(page);

				// Settings > Models and roles: the same list in a card, the name to save under, and what
				// a usage limit does.
				await page.keyboard.press('Control+.');
				const win = page.locator('[role=dialog][data-settings]');
				await expect(win).toBeVisible();
				await win.getByRole('tab', { name: 'Models and roles', exact: true }).click();
				await expect(win.locator('[data-pane="models"]')).toBeVisible();
				await expect(win.locator('[aria-busy=true]')).toHaveCount(0);
				const card = win.locator('[data-profiles]');
				await expect(card.locator('[data-profile="mine"]')).toHaveAttribute('aria-current', 'true');
				await expect(card.locator('[data-profile="mine"]')).toContainText('in use');
				await expect(
					card.getByRole('button', { name: 'Switch to the profile gpt' })
				).toBeDisabled();
				await win
					.getByRole('heading', { name: 'Profiles' })
					.evaluate((el) => el.scrollIntoView({ block: 'start' }));
				await page.mouse.move(width - 2, 2);
				await capture('settings-profiles');
				const name = win.getByLabel('Save current as…');
				await name.evaluate((el) => el.scrollIntoView({ block: 'center' }));
				await name.fill('kept');
				await page.mouse.move(width - 2, 2);
				await capture('settings-save');
				await win.getByRole('button', { name: 'Save', exact: true }).click();
				const toast = page.locator('[data-sonner-toast]');
				await expect(toast).toContainText('Saved the roles as the profile kept.');
				await expect(card.locator('[data-profile="kept"]')).toContainText('your config');
				await expect(name).toHaveValue('');
				await expect(toast).toHaveCount(0, { timeout: 20_000 });
				const limit = win.locator('[data-profile-setting="on_limit"]');
				await expect(limit).toContainText('fallback');
				await expect(win.locator('[data-profile-setting="on_reset"]')).toContainText('ask');
				await limit.evaluate((el) => el.scrollIntoView({ block: 'center' }));
				await page.mouse.move(width - 2, 2);
				await capture('settings-on-limit');
				await closeAll(page);

				// The next message runs out of allowance: the session moves to the fallback, the chip
				// follows and a notice says why, until when, and what the next reply costs.
				await box.fill('go on');
				await box.press('Enter');
				await expect(toast).toContainText('Switched to the profile fallback.');
				await expect(toast).toContainText('mock reached its');
				await expect(toast).toContainText('(in 2 h)');
				await expect(toast).toContainText('new prompt cache');
				await expect(chip).toHaveAccessibleName('Profile: fallback');
				await expect(
					page.getByRole('paragraph').filter({ hasText: 'On the fallback.' })
				).toBeVisible();
				await page.mouse.move(width - 2, 2);
				await capture('limit-toast');
				await toastsClear(page, 'limit-toast');
			} finally {
				writeFileSync(join(dir, 'measured.json'), JSON.stringify(measured, null, '\t'));
				await stack.stop();
			}

			const report = [...where].map(([f, states]) => `${f}  [${states.join(', ')}]`).sort();
			expect(report.join('\n'), `${basename(dir)}: ${report.length} findings`).toBe('');
		});

/** The title of the page the agent's browser is on: too long for one line of the panel. */
const SITE_TITLE =
	'Release notes: what changed in the web page, the terminal and the browser tool since the last version, and what to do before updating';
/** Its path: with it the address is too long for the panel, and for the field it is typed into. */
const SITE_PATH =
	'releases/notes/web-page-terminal-and-browser-tool?from=0.15.0&to=0.16.0&show=added,changed,fixed';
const SITE = `<!doctype html>
<html lang="en">
	<head>
		<meta charset="utf-8" />
		<title>${SITE_TITLE}</title>
		<style>
			body { margin: 0; font: 18px/1.5 system-ui, sans-serif; color: #18181b; background: #fff; }
			header { padding: 24px 40px; background: #18181b; color: #fafafa; }
			h1 { margin: 0; font-size: 28px; }
			main { padding: 16px 40px; }
			li { margin: 6px 0; }
		</style>
	</head>
	<body>
		<header><h1>Release notes</h1></header>
		<main>
			<p>What changed since the last version.</p>
			<ul>
				<li>The Browser tab shows the page the agent is on, live.</li>
				<li>Take over to click and type in it, and hand back.</li>
				<li>A sheet's buttons stand in a row in a wide window.</li>
			</ul>
		</main>
	</body>
</html>`;

interface Box {
	left: number;
	top: number;
	right: number;
	bottom: number;
	width: number;
	height: number;
}
/** The Browser tab as it is laid out: where each of its parts is. */
interface BrowserBoxes {
	status: string;
	viewport: { width: number; height: number };
	/** The box that scrolls the panel, inside its border: the panel reaches its edges. */
	room: Box;
	panel: Box;
	/** The parts of the panel that stand under each other, from the top, by the attribute of each. */
	parts: Record<string, Box>;
	/** The parts of the header row, from the left: the globe, the title block, the badge, the buttons. */
	head: Record<string, Box>;
	/** The icon buttons of the header, by their accessible names. */
	buttons: (Box & { name: string; radius: string })[];
	/** The field the address is typed into, while this device holds the browser. */
	input: Box | null;
	/** The border box of the group around that field, and the buttons of the address row. */
	group: Box | null;
	controls: (Box & { name: string; radius: string })[];
	/** The button of the empty state that opens the browser, on a device that may control. */
	open: Box | null;
	/** How far anything inside that group reaches past its border box. */
	overhang: { x: number; y: number } | null;
	/** The picture the host sent, the box it is drawn in, and how it is fitted to that box. */
	img: (Box & { naturalWidth: number; naturalHeight: number; fit: string }) | null;
	/** What stands in the place of the picture until the first one arrives. */
	waiting: string | null;
	/** What is drawn around the picture's box: nothing, it is no card. */
	frame: { border: string; radius: string; background: string } | null;
	/** The lines that are cut to the width of the panel: how wide their text is, and how it ends. */
	cut: Record<string, { text: number; box: number; ends: string }>;
	/** Whether the keyboard is on the picture. */
	focused: boolean;
	/**
	 * How far the panel's scroller, and the page, can be scrolled sideways; and how much wider than
	 * the panel what is in it is (the panel scrolls nothing itself, so this is only told).
	 */
	sideways: { scroller: number; page: number };
	widerThanPanel: number;
	/** How much of the picture is outside what its scroller shows. */
	pictureHidden: number;
}

/** Runs in the page: measures the Browser tab that is on screen. */
function browserBoxes(): BrowserBoxes {
	const round = (n: number) => Math.round(n * 100) / 100;
	const box = (r: { left: number; top: number; right: number; bottom: number }): Box => ({
		left: round(r.left),
		top: round(r.top),
		right: round(r.right),
		bottom: round(r.bottom),
		width: round(r.right - r.left),
		height: round(r.bottom - r.top)
	});
	const of = (el: Element) => box(el.getBoundingClientRect());
	const panel = Array.from(document.querySelectorAll<HTMLElement>('[data-browser]')).find((el) =>
		el.checkVisibility()
	)!;
	let scroller = panel.parentElement!;
	while (scroller.parentElement && !/auto|scroll/.test(getComputedStyle(scroller).overflowY))
		scroller = scroller.parentElement;
	const outer = scroller.getBoundingClientRect();
	const left = outer.left + scroller.clientLeft;
	const top = outer.top + scroller.clientTop;
	const room = box({
		left,
		top,
		right: left + scroller.clientWidth,
		bottom: top + scroller.clientHeight
	});
	const marked = (names: string[]) => {
		const found: Record<string, Box> = {};
		for (const name of names) {
			const el = panel.querySelector(`[data-browser-${name}]`);
			if (el && el.checkVisibility()) found[name] = of(el);
		}
		return found;
	};
	const parts = marked(['header', 'error', 'empty', 'address', 'picture', 'note']);
	const opener = panel.querySelector('[data-browser-open]');
	const head = marked(['heading', 'status', 'actions']);
	const globe = panel.querySelector('[data-browser-header] > svg');
	if (globe?.checkVisibility()) head.globe = of(globe);
	const cut: BrowserBoxes['cut'] = {};
	for (const name of ['title', 'url']) {
		const el = panel.querySelector<HTMLElement>(`[data-browser-${name}]`);
		if (el)
			cut[name] = {
				text: el.scrollWidth,
				box: el.clientWidth,
				ends: getComputedStyle(el).textOverflow
			};
	}
	const picture = panel.querySelector<HTMLElement>('[data-browser-picture]');
	const img = picture?.querySelector('img');
	const frame = picture ? getComputedStyle(picture) : null;
	const input = panel.querySelector('[data-browser-address] input');
	const group = input?.closest('[data-slot=input-group]');
	const overhang = { x: 0, y: 0 };
	if (group) {
		const g = group.getBoundingClientRect();
		for (const el of Array.from(group.querySelectorAll('*'))) {
			const r = el.getBoundingClientRect();
			overhang.x = round(Math.max(overhang.x, r.right - g.right, g.left - r.left));
			overhang.y = round(Math.max(overhang.y, r.bottom - g.bottom, g.top - r.top));
		}
	}
	const shown = { top: Math.max(outer.top, 0), bottom: Math.min(outer.bottom, window.innerHeight) };
	const p = picture?.getBoundingClientRect();
	return {
		status: panel.dataset.status ?? '',
		viewport: { width: window.innerWidth, height: window.innerHeight },
		room,
		panel: of(panel),
		parts,
		head,
		buttons: Array.from(panel.querySelectorAll('[data-browser-actions] button')).map((b) => ({
			name: b.getAttribute('aria-label') ?? '',
			radius: getComputedStyle(b).borderRadius,
			...of(b)
		})),
		input: input ? of(input) : null,
		group: group ? of(group) : null,
		overhang: group ? overhang : null,
		controls: Array.from(panel.querySelectorAll('[data-browser-address] button')).map((b) => ({
			name: b.getAttribute('aria-label') ?? (b.textContent ?? '').trim(),
			radius: getComputedStyle(b).borderRadius,
			...of(b)
		})),
		open: opener ? of(opener) : null,
		img: img
			? {
					...of(img),
					naturalWidth: img.naturalWidth,
					naturalHeight: img.naturalHeight,
					fit: getComputedStyle(img).objectFit
				}
			: null,
		waiting: picture?.querySelector('[data-browser-waiting]')?.textContent?.trim() ?? null,
		frame: frame && {
			border: frame.borderWidth,
			radius: frame.borderRadius,
			background: frame.backgroundColor
		},
		cut,
		// The keyboard is on the field inside the picture that takes it for the page.
		focused:
			!!picture && picture !== document.activeElement && picture.contains(document.activeElement),
		sideways: {
			scroller: scroller.scrollWidth - scroller.clientWidth,
			page: document.documentElement.scrollWidth - document.documentElement.clientWidth
		},
		widerThanPanel: panel.scrollWidth - panel.clientWidth,
		pictureHidden: p ? round(Math.max(p.bottom - shown.bottom, shown.top - p.top, 0)) : 0
	};
}

/**
 * What has to hold of the Browser tab in every state, at every size. `fill`: the panel is in the
 * pane beside the chat, as tall as the pane, and not in a sheet that is as tall as what is in it.
 */
function browserHolds(b: BrowserBoxes, state: string, fill: boolean) {
	const said = (what: string) => `${state}: ${what}: ${JSON.stringify(b)}`;
	const near = 0.5;
	expect(b.sideways, said('nothing scrolls sideways')).toEqual({ scroller: 0, page: 0 });
	expect(b.widerThanPanel, said('nothing is wider than the panel')).toBe(0);
	// Edge to edge: the panel is as wide as what it is in (a pixel of rounding: the room is whole).
	expect(Math.abs(b.panel.left - b.room.left), said('the panel starts at the edge')).toBeLessThan(
		1
	);
	expect(Math.abs(b.panel.right - b.room.right), said('the panel ends at the edge')).toBeLessThan(
		1
	);
	// One header, 48px tall, then the rest: every part spans the panel, under the part before it.
	expect(b.parts.header, said('the header is there')).toBeDefined();
	expect(Math.abs(b.parts.header.top - b.panel.top), said('the header is first')).toBeLessThan(1.5);
	expect(Math.abs(b.parts.header.height - 48), said('the header is one row')).toBeLessThan(near);
	let above = b.panel.top;
	for (const [name, part] of Object.entries(b.parts)) {
		expect(Math.abs(part.left - b.panel.left), said(`${name} starts at the edge`)).toBeLessThan(
			near
		);
		expect(Math.abs(part.right - b.panel.right), said(`${name} ends at the edge`)).toBeLessThan(
			near
		);
		expect(part.top, said(`${name} is under what is above it`)).toBeGreaterThanOrEqual(
			above - near
		);
		above = part.bottom;
	}
	// The header row: the globe, the title block, the badge, the buttons, in that order, none on
	// another, each inside the header and around its middle.
	const header = b.parts.header;
	const middle = (r: Box) => r.top + r.height / 2;
	let before = header.left;
	for (const name of ['globe', 'heading', 'status', 'actions']) {
		const part = b.head[name];
		if (!part) continue;
		expect(part.left, said(`${name} is beside what is before it`)).toBeGreaterThanOrEqual(
			before - near
		);
		expect(part.right, said(`${name} ends in the header`)).toBeLessThanOrEqual(header.right + near);
		expect(part.top, said(`${name} is under the top of the header`)).toBeGreaterThanOrEqual(
			header.top - near
		);
		expect(part.bottom, said(`${name} is over the bottom of the header`)).toBeLessThanOrEqual(
			header.bottom + near
		);
		expect(
			Math.abs(middle(part) - middle(header)),
			said(`${name} is in the middle of the header`)
		).toBeLessThanOrEqual(1);
		before = part.right;
	}
	expect(b.head.heading.width, said('the title has room')).toBeGreaterThan(40);
	// The buttons: one row, squares of one size and one corner, none on another.
	before = header.left;
	for (const button of b.buttons) {
		expect(
			Math.abs(button.top - b.buttons[0].top),
			said(`${button.name} is on the row`)
		).toBeLessThan(near);
		expect(
			Math.abs(button.height - b.buttons[0].height),
			said(`${button.name} is as tall as the others`)
		).toBeLessThan(near);
		expect(Math.abs(button.width - button.height), said(`${button.name} is square`)).toBeLessThan(
			near
		);
		expect(button.radius, said(`${button.name} has the corners of the others`)).toBe(
			b.buttons[0].radius
		);
		expect(button.left, said(`${button.name} is beside the one before`)).toBeGreaterThanOrEqual(
			before - near
		);
		expect(button.right, said(`${button.name} ends in the header`)).toBeLessThanOrEqual(
			header.right + near
		);
		before = button.right;
	}
	// The button that opens the browser: in the empty state, in the middle of it, as tall as any
	// button with a word on it (32px).
	if (b.open) {
		const empty = b.parts.empty;
		expect(
			b.open.left >= empty.left &&
				b.open.right <= empty.right &&
				b.open.top >= empty.top &&
				b.open.bottom <= empty.bottom,
			said('Open is inside the empty state')
		).toBe(true);
		expect(
			Math.abs(b.open.left + b.open.width / 2 - (empty.left + empty.width / 2)),
			said('Open is in the middle')
		).toBeLessThanOrEqual(1);
		expect(Math.abs(b.open.height - 32), said('Open is 32px tall')).toBeLessThan(near);
		expect(b.open.width, said('Open is at least 6rem wide')).toBeGreaterThanOrEqual(96 - near);
	}
	const picture = b.parts.picture;
	if (!picture) return;
	// No card: nothing is drawn around the picture's box, and nothing behind it.
	expect(b.frame, said('the picture has no box of its own')).toEqual({
		border: '0px',
		radius: '0px',
		background: 'rgba(0, 0, 0, 0)'
	});
	// The box has the shape of the picture (16:9 until one arrives) at the width of the panel, until
	// it would be taller than what is left of the pane, or than 70% of the window in a sheet; the
	// picture is whole inside it.
	let shape = 9 / 16;
	if (b.img) {
		const img = b.img;
		expect(img.naturalWidth, said('the picture has a width')).toBeGreaterThan(0);
		expect(img.fit, said('the picture keeps its shape')).toBe('contain');
		shape = img.naturalHeight / img.naturalWidth;
		for (const [edge, inside] of [
			['left', img.left >= picture.left - near],
			['top', img.top >= picture.top - near],
			['right', img.right <= picture.right + near],
			['bottom', img.bottom <= picture.bottom + near]
		] as const)
			expect(inside, said(`the picture's ${edge} is inside its box`)).toBe(true);
	} else
		expect(b.waiting, said('its place says a picture is on its way')).toBe(
			'Waiting for the first picture…'
		);
	const most = fill
		? b.room.bottom - picture.top - (b.parts.note?.height ?? 0)
		: 0.7 * b.viewport.height;
	const shaped = picture.width * shape;
	expect(
		Math.abs(picture.height - Math.min(shaped, most)),
		said(`the picture is ${Math.min(shaped, most)} tall`)
	).toBeLessThan(1);
}

// The Browser tab: before the agent has a browser, while it shows one, and while this device holds it.
for (const view of VIEWS)
	for (const theme of THEMES)
		at(view, `the Browser tab holds at ${view.name}, ${theme}`, async ({ page }) => {
			const { width, height } = view;
			const wide = width >= 1024;
			const dir = join(SHOTS, `${view.name}-${theme}-browser`);
			mkdirSync(dir, { recursive: true });
			const measured: Record<string, Measured> = {};
			const boxes: Record<string, BrowserBoxes> = {};
			const where = new Map<string, string[]>();
			let shot = 0;
			const panel = page.locator('[data-browser]:visible');
			const capture = async (state: string) => {
				await page.mouse.move(width - 2, 2);
				await settle(page);
				const m = await page.evaluate(audit, 'body');
				const file = `${String(++shot).padStart(2, '0')}-${state}`;
				await page.screenshot({ path: join(dir, `${file}.png`) });
				measured[file] = m;
				for (const f of m.findings) where.set(f, [...(where.get(f) ?? []), state]);
				expect(m.rows, `${state}: rows measured`).toBeGreaterThan(0);
				expect(m.texts, `${state}: texts measured`).toBeGreaterThan(0);
				const b = await page.evaluate(browserBoxes);
				boxes[file] = b;
				browserHolds(b, state, wide);
				return b;
			};
			/** The Browser tab of the pane beside the chat, or of the sheet a narrow window has for it. */
			const openTab = async () => {
				if (wide) {
					const pane = page.locator('[data-right-pane]');
					await expect(pane).toHaveAttribute('data-right-pane', 'open');
					await pane.getByRole('tab', { name: 'Browser' }).click();
				} else {
					await page.getByRole('button', { name: 'Agents, usage and browser' }).click();
					await page
						.getByRole('dialog', { name: 'Agents' })
						.getByRole('radio', { name: 'Browser' })
						.click();
					await expect(page.getByRole('dialog', { name: 'Browser' })).toBeVisible();
				}
				await expect(panel).toBeVisible();
			};
			/** Leaves the Browser tab and comes back: the host sends a picture to a device that watches anew. */
			const comeBack = async () => {
				if (wide) {
					const pane = page.locator('[data-right-pane]');
					await pane.getByRole('tab', { name: 'Agents' }).click();
					await expect(panel).toHaveCount(0);
					await pane.getByRole('tab', { name: 'Browser' }).click();
				} else {
					const sheet = page.getByRole('dialog');
					await sheet.getByRole('radio', { name: 'Agents' }).click();
					await expect(panel).toHaveCount(0);
					await sheet.getByRole('radio', { name: 'Browser' }).click();
				}
				await expect(panel).toBeVisible();
			};
			// The pictures the host sends are held back while `pictures` is false: that is the panel
			// of a browser whose first picture has not arrived.
			let pictures = true;
			await page.routeWebSocket(/./, (ws) => {
				const server = ws.connectToServer();
				ws.onMessage((m) => server.send(m));
				server.onMessage((m) => {
					if (pictures || typeof m !== 'string' || !m.includes('"type":"browser_frame"'))
						ws.send(m);
				});
			});

			const site = await serve(SITE);
			const address = site.url + SITE_PATH;
			const stack = new Stack();
			try {
				await stack.start('auto', false, [], DONE);
				const idle = stack.sessionId;
				await page.setViewportSize({ width, height });
				await page.addInitScript((t) => localStorage.setItem('azazel-theme', t), theme);
				await page.goto(stack.pair);
				await expect(page.locator('[data-session-list]:visible a').first()).toBeVisible();
				const origin = new URL(page.url()).origin;
				const open = async (id: string, reply: string) => {
					await page.goto(`${origin}/s/${id}`);
					await expect(page.locator('html')).toHaveClass(new RegExp(`\\b${theme}\\b`));
					await expect(page.getByRole('paragraph').filter({ hasText: reply })).toBeVisible();
					await expect(page.getByRole('combobox', { name: 'Message' })).toBeVisible();
					await openTab();
				};

				// No browser: the header with the one button there is something to do with, and a plain
				// line that says so. No badge, no picture, nothing that looks like a page.
				await open(idle, 'Done.');
				await expect(panel).toHaveAttribute('data-status', 'none');
				await expect(panel.locator('[data-browser-empty]')).toContainText(
					'The agent has no browser open'
				);
				await expect(panel.getByRole('status')).toHaveCount(0);
				await expect(panel.getByRole('button', { name: 'Take over' })).toHaveCount(0);
				const none = await capture('none');
				expect(none.buttons.map((b) => b.name)).toEqual(['Sign in']);
				expect(none.parts.picture, 'no picture without a browser').toBeUndefined();
				expect(Object.keys(none.parts), 'the header and the empty state').toEqual([
					'header',
					'empty'
				]);

				if (!agentBrowserFound) {
					test.info().annotations.push({
						type: 'not measured',
						description:
							'waiting, watching and holding: no Chromium-based browser on this machine for the agent to open'
					});
				} else {
					const browsing = stack.session('open the page', browses(address, 'Opened the page.'));
					await stack.until({ [browsing]: 'idle' });

					// Waiting: the browser is on a page and no picture of it has come. Its place says so.
					pictures = false;
					await open(browsing, 'Opened the page.');
					await expect(page.locator('[data-status="error"]')).toHaveCount(0);
					await expect(panel).toHaveAttribute('data-status', 'watching');
					await expect(panel.locator('[data-browser-title]')).toHaveText(SITE_TITLE);
					await expect(panel.locator('[data-browser-waiting]')).toHaveText(
						'Waiting for the first picture…'
					);
					const waiting = await capture('waiting');
					expect(waiting.img, 'no picture yet').toBeNull();
					pictures = true;
					await comeBack();

					// Watching: one header (the title over the address, each on one line, the badge, the
					// buttons) over the picture, which takes the rest.
					await expect(panel).toHaveAttribute('data-status', 'watching');
					await expect(panel.getByRole('status')).toHaveText('Watching');
					await expect(panel.locator('[data-browser-title]')).toHaveText(SITE_TITLE);
					await expect(panel.locator('[data-browser-url]')).toHaveText(address);
					const img = panel.locator('[data-browser-picture]').getByRole('img');
					await expect
						.poll(() => img.evaluate((el: HTMLImageElement) => el.naturalWidth))
						.toBeGreaterThan(0);
					const watching = await capture('watching');
					expect(watching.waiting, 'the picture took its place').toBeNull();
					expect(watching.buttons.map((b) => b.name)).toEqual(['Take over', 'Sign in']);
					expect(Object.keys(watching.parts), 'the header and the picture').toEqual([
						'header',
						'picture'
					]);
					expect(
						Math.abs(watching.parts.picture.top - watching.parts.header.bottom),
						'the picture starts under the header'
					).toBeLessThan(0.5);
					for (const line of ['title', 'url'] as const) {
						expect(watching.cut[line].text, `the ${line} is longer than its line`).toBeGreaterThan(
							watching.cut[line].box
						);
						expect(watching.cut[line].ends, `the ${line} ends in an ellipsis`).toBe('ellipsis');
					}
					expect(watching.focused, 'the picture takes no keys while watching').toBe(false);

					// Holding: Hand back in place of Take over, a row for the address under the header,
					// the keyboard on the picture, and under it what holding means and how to leave.
					await panel.getByRole('button', { name: 'Take over' }).click();
					await expect(panel).toHaveAttribute('data-status', 'you');
					await expect(panel.getByRole('status')).toHaveText('Holding');
					await expect(panel.getByRole('textbox', { name: 'Address' })).toHaveValue(address);
					await expect(
						panel
							.getByRole('application')
							.getByRole('textbox', { name: "Type into the agent's page" })
					).toBeFocused();
					const holding = await capture('holding');
					expect(holding.buttons.map((b) => b.name)).toEqual(['Hand back', 'Sign in']);
					expect(holding.focused, 'the keyboard is on the picture').toBe(true);
					expect(
						Object.keys(holding.parts),
						'the header, the address, the picture, the note'
					).toEqual(['header', 'address', 'picture', 'note']);
					// Back, Forward and Reload stand before the group (over it in a narrow panel), as
					// tall as it; the address and Go are inside its border, in that order.
					addressHolds(holding, 'holding');
					// Taking over moved nothing in the header: Hand back is where Take over was in it. (A
					// sheet grows from the bottom of the window, so the header itself has moved there.)
					const placed = (b: BrowserBoxes) => {
						const [first] = b.buttons;
						const header = b.parts.header;
						return {
							left: first.left - header.left,
							top: first.top - header.top,
							width: first.width,
							height: first.height
						};
					};
					for (const edge of ['left', 'top', 'width', 'height'] as const)
						expect(
							Math.abs(placed(holding)[edge] - placed(watching)[edge]),
							`Hand back has the ${edge} of Take over`
						).toBeLessThan(0.5);

					// The picture, where a short window starts it below the fold.
					if (holding.pictureHidden > 0) {
						await panel.locator('[data-browser-picture]').scrollIntoViewIfNeeded();
						await capture('holding-picture');
					}
				}
			} finally {
				writeFileSync(join(dir, 'measured.json'), JSON.stringify(measured, null, '\t'));
				writeFileSync(join(dir, 'boxes.json'), JSON.stringify(boxes, null, '\t'));
				await stack.stop();
				await site.close();
			}

			const report = [...where].map(([f, states]) => `${f}  [${states.join(', ')}]`).sort();
			expect(report.join('\n'), `${basename(dir)}: ${report.length} findings`).toBe('');
		});

/** What has to hold of the address row, which is there while this device holds the browser. */
function addressHolds(b: BrowserBoxes, state: string) {
	const said = (what: string) => `${state}: ${what}: ${JSON.stringify(b)}`;
	const near = 0.5;
	const row = b.parts.address;
	const group = b.group!;
	expect(
		b.controls.map((c) => c.name),
		said('the buttons of the row')
	).toEqual(['Back', 'Forward', 'Reload', 'Go']);
	const [back, forward, reload, go] = b.controls;
	// Back, Forward and Reload: one line, squares as tall as the field, with one corner, none on
	// another.
	let end = row.left;
	for (const c of [back, forward, reload]) {
		expect(c.left, said(`${c.name} starts after what is before it`)).toBeGreaterThanOrEqual(end);
		expect(Math.abs(c.top - back.top), said(`${c.name} is on the line of Back`)).toBeLessThan(near);
		expect(
			Math.abs(c.height - group.height),
			said(`${c.name} is as tall as the field`)
		).toBeLessThan(near);
		expect(Math.abs(c.width - c.height), said(`${c.name} is square`)).toBeLessThan(near);
		expect(c.radius, said(`${c.name} has the corners of Back`)).toBe(back.radius);
		end = c.right;
	}
	expect(back.top, said('the buttons are under the top of the row')).toBeGreaterThanOrEqual(
		row.top
	);
	// A panel narrower than 24rem gives the field the line under them, as wide as the row inside
	// its padding, so that an address can be read there; a wider one has it beside them.
	const under = b.panel.width < 384;
	if (under) {
		expect(group.top, said('the field is under the buttons')).toBeGreaterThanOrEqual(back.bottom);
		expect(
			Math.abs(group.left - back.left),
			said('the field starts where Back starts')
		).toBeLessThan(near);
		expect(
			Math.abs(row.right - group.right - (group.left - row.left)),
			said('the field is as far from one edge as from the other')
		).toBeLessThan(near);
	} else {
		expect(
			Math.abs(group.top - back.top),
			said('the field is on the line of the buttons')
		).toBeLessThan(near);
		expect(group.left, said('the field starts after Reload')).toBeGreaterThanOrEqual(end);
	}
	expect(group.right, said('the field ends in the panel')).toBeLessThanOrEqual(row.right);
	expect(group.bottom, said('the field is over the bottom of the row')).toBeLessThanOrEqual(
		row.bottom
	);
	expect(group.width, said('the field has room for an address')).toBeGreaterThan(under ? 180 : 100);
	// The address and Go are inside the border of the group, in that order, none on another.
	end = group.left;
	for (const c of [{ name: 'the address', ...b.input! }, go]) {
		expect(c.left, said(`${c.name} starts after what is before it`)).toBeGreaterThanOrEqual(end);
		expect(c.top, said(`${c.name} is under the top of the group`)).toBeGreaterThanOrEqual(
			group.top
		);
		expect(c.bottom, said(`${c.name} is over the bottom of the group`)).toBeLessThanOrEqual(
			group.bottom
		);
		end = c.right;
	}
	expect(end, said('Go ends in the group')).toBeLessThanOrEqual(group.right);
	// Nothing reaches past the border of the group: not a pixel.
	expect(b.overhang, said('what reaches past the group')).toEqual({ x: 0, y: 0 });
}

/** Opens the Browser tab of the pane beside the chat, or of the sheet a narrow window has for it. */
async function showBrowserTab(page: Page, wide: boolean) {
	if (wide) {
		const pane = page.locator('[data-right-pane]');
		await expect(pane).toHaveAttribute('data-right-pane', 'open');
		await pane.getByRole('tab', { name: 'Browser' }).click();
	} else {
		await page.getByRole('button', { name: 'Agents, usage and browser' }).click();
		await page
			.getByRole('dialog', { name: 'Agents' })
			.getByRole('radio', { name: 'Browser' })
			.click();
		await expect(page.getByRole('dialog', { name: 'Browser' })).toBeVisible();
	}
	await expect(page.locator('[data-browser]:visible')).toBeVisible();
}

/** A tooltip as it is laid out, and the window it has to be inside. */
interface TipBox extends Box {
	text: string;
	window: { width: number; height: number };
	/** How much wider than its box what is in it is. */
	wider: number;
}
/** Runs in the page: the tooltip that is open. */
function tooltipBox(): TipBox | null {
	const tip = Array.from(document.querySelectorAll<HTMLElement>('[data-slot=tooltip-content]'))
		.filter((el) => el.checkVisibility())
		.pop();
	if (!tip) return null;
	const r = tip.getBoundingClientRect();
	const round = (n: number) => Math.round(n * 100) / 100;
	return {
		text: (tip.textContent ?? '').trim(),
		left: round(r.left),
		top: round(r.top),
		right: round(r.right),
		bottom: round(r.bottom),
		width: round(r.width),
		height: round(r.height),
		window: { width: window.innerWidth, height: window.innerHeight },
		wider: tip.scrollWidth - tip.clientWidth
	};
}

/** The ring of the element that has the focus. */
interface Ring {
	name: string;
	/** Whether the focus is one a keyboard brought: only that one is drawn. */
	keyboard: boolean;
	shadow: string;
	/** The box that hides a part of the ring, when one does. */
	cutBy: string | null;
}
/** Runs in the page: the ring around what Tab reached, 3px wide, and whether all of it shows. */
function focusRing(): Ring | null {
	const el = document.activeElement as HTMLElement | null;
	if (!el || el === document.body) return null;
	const wide = 3;
	const r = el.getBoundingClientRect();
	let cutBy: string | null = null;
	for (let p = el.parentElement; p && !cutBy; p = p.parentElement) {
		const s = getComputedStyle(p);
		if (s.overflowX === 'visible' && s.overflowY === 'visible') continue;
		const box = p.getBoundingClientRect();
		if (box.width === 0 && box.height === 0) continue;
		if (
			r.left - wide < box.left ||
			r.right + wide > box.right ||
			r.top - wide < box.top ||
			r.bottom + wide > box.bottom
		)
			cutBy = `${p.tagName.toLowerCase()}.${String(p.className).split(' ')[0]}`;
	}
	return {
		name: el.getAttribute('aria-label') ?? (el.textContent ?? '').trim(),
		keyboard: el.matches(':focus-visible'),
		shadow: getComputedStyle(el).boxShadow,
		cutBy
	};
}

/** The Browser tab of a device that may not watch. */
interface DeniedBoxes {
	panel: Box;
	denied: Box;
	title: Box;
	text: Box;
	/** The controls in the panel: none, there is nothing such a device can do there. */
	controls: number;
	sideways: { page: number; panel: number };
}
/** Runs in the page: measures the Browser tab of a device that was refused the pictures. */
function deniedBoxes(): DeniedBoxes {
	const round = (n: number) => Math.round(n * 100) / 100;
	const of = (el: Element): Box => {
		const r = el.getBoundingClientRect();
		return {
			left: round(r.left),
			top: round(r.top),
			right: round(r.right),
			bottom: round(r.bottom),
			width: round(r.width),
			height: round(r.height)
		};
	};
	const panel = Array.from(document.querySelectorAll<HTMLElement>('[data-browser]')).find((el) =>
		el.checkVisibility()
	)!;
	const denied = panel.querySelector('[data-browser-denied]')!;
	return {
		panel: of(panel),
		denied: of(denied),
		title: of(denied.querySelector('[data-slot=empty-title]')!),
		text: of(denied.querySelector('[data-slot=empty-description]')!),
		controls: panel.querySelectorAll('button, input, a').length,
		sideways: {
			page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
			panel: panel.scrollWidth - panel.clientWidth
		}
	};
}

/** What the tooltip of each button of the Browser tab begins with. */
const TIPS: Record<string, string> = {
	'Take over': 'Take over: the agent waits while you click and type',
	'Hand back': 'Hand back to the agent',
	'Sign in': "Sign in: opens the agent's browser in a window",
	Back: 'Back',
	Forward: 'Forward',
	Reload: 'Reload'
};

// The states of the Browser tab a scripted agent never brings about: a browser opened from the page,
// on its blank page; a tooltip under the pointer; the ring of a button Tab reached; an address that
// is typed, a step back and one forward; what the host refuses; another device that holds the
// browser; and a view link, which may not watch.
for (const view of VIEWS)
	for (const theme of THEMES)
		at(
			view,
			`the Browser tab's other states hold at ${view.name}, ${theme}`,
			async ({ page, browser }) => {
				test.setTimeout(180_000);
				const { width, height } = view;
				const wide = width >= 1024;
				const dir = join(SHOTS, `${view.name}-${theme}-browser-states`);
				mkdirSync(dir, { recursive: true });
				const measured: Record<string, Measured> = {};
				const boxes: Record<string, unknown> = {};
				const where = new Map<string, string[]>();
				let shot = 0;
				const panelOf = (on: Page) => on.locator('[data-browser]:visible');
				const panel = panelOf(page);
				const title = panel.locator('[data-browser-title]');
				const button = (name: string, on = panel) => on.getByRole('button', { name, exact: true });
				const slug = (name: string) => name.toLowerCase().replace(/\s+/g, '-');
				const escaped = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
				const tip = (name: string) =>
					page
						.locator('[data-slot=tooltip-content]')
						.filter({ hasText: new RegExp(`^${escaped(TIPS[name])}`) });
				/** Screenshots `on` and looks for what does not hold; `rest` first takes the pointer away. */
				const audited = async (state: string, on = page, rest = true) => {
					if (rest) await on.mouse.move(width - 2, 2);
					await settle(on);
					const m = await on.evaluate(audit, 'body');
					const file = `${String(++shot).padStart(2, '0')}-${state}`;
					await on.screenshot({ path: join(dir, `${file}.png`) });
					measured[file] = m;
					for (const f of m.findings) where.set(f, [...(where.get(f) ?? []), state]);
					expect(m.texts, `${state}: texts measured`).toBeGreaterThan(0);
					return file;
				};
				const capture = async (state: string) => {
					const file = await audited(state);
					const b = await page.evaluate(browserBoxes);
					boxes[file] = b;
					browserHolds(b, state, wide);
					return b;
				};
				/** The tab that chooses this panel: Tab goes on from it into the panel. */
				const chooser = wide
					? page.locator('[data-right-pane]').getByRole('tab', { name: 'Browser' })
					: page.getByRole('dialog').getByRole('radio', { name: 'Browser' });
				/** Presses Tab until `name` has the focus, and measures the ring it has then. */
				const tabTo = async (name: string) => {
					const target = button(name);
					for (let presses = 0; presses < 16; presses++) {
						if (await target.evaluate((el) => el === document.activeElement)) break;
						await page.keyboard.press('Tab');
					}
					await expect(target).toBeFocused();
					// Tab opens the tooltip too: it names the icon for someone who has no pointer.
					await expect(tip(name)).toBeVisible();
					const file = await audited(`focus-${slug(name)}`, page, false);
					const ring = await page.evaluate(focusRing);
					boxes[file] = ring;
					expect(ring, `${name}: the ring Tab brought`).toMatchObject({
						name,
						keyboard: true,
						cutBy: null
					});
					expect(ring!.shadow, `${name}: a ring is drawn`).not.toBe('none');
				};

				const site = await serve(SITE);
				const address = site.url + SITE_PATH;
				const stack = new Stack();
				const others: BrowserContext[] = [];
				try {
					await stack.start('auto', false, [], DONE);
					const id = stack.sessionId;
					await page.setViewportSize({ width, height });
					await page.addInitScript((t) => localStorage.setItem('azazel-theme', t), theme);
					await page.goto(stack.pair);
					await expect(page.locator('[data-session-list]:visible a').first()).toBeVisible();
					const origin = new URL(page.url()).origin;
					/** Opens the session on a device, and the Browser tab there. */
					const open = async (on: Page, big: boolean) => {
						await on.goto(`${origin}/s/${id}`);
						await expect(on.getByRole('paragraph').filter({ hasText: 'Done.' })).toBeVisible();
						await showBrowserTab(on, big);
					};
					await open(page, wide);
					await expect(page.locator('html')).toHaveClass(new RegExp(`\\b${theme}\\b`));

					// No browser: a device that may control has a button that opens one.
					await expect(panel).toHaveAttribute('data-status', 'none');
					await expect(button('Open')).toBeVisible();
					const none = await capture('none');
					expect(none.open, 'a control device can open the browser').not.toBeNull();
					expect(Object.keys(none.parts), 'the header and the empty state').toEqual([
						'header',
						'empty'
					]);

					if (!agentBrowserFound) {
						test.info().annotations.push({
							type: 'not measured',
							description:
								'the blank page, tooltips, focus rings, the address row, the alert and a browser held elsewhere: no Chromium-based browser on this machine for the host to open'
						});
					} else {
						// Open: the host starts the browser, and its blank page is watched like any other.
						await button('Open').click();
						await expect(panel).toHaveAttribute('data-status', 'watching');
						await expect(title).toHaveText('Blank page');
						await expect(panel.locator('[data-browser-url]')).toHaveText('about:blank');
						const img = panel.locator('[data-browser-picture]').getByRole('img');
						await expect
							.poll(() => img.evaluate((el: HTMLImageElement) => el.naturalWidth))
							.toBeGreaterThan(0);
						await expect(button('Open')).toHaveCount(0);
						const blank = await capture('blank');
						expect(blank.open, 'nothing is left to open').toBeNull();
						expect(blank.buttons.map((b) => b.name)).toEqual(['Take over', 'Sign in']);
						expect(Object.keys(blank.parts), 'the header and the picture').toEqual([
							'header',
							'picture'
						]);

						// A tooltip under the pointer: all of it in the window, and not on the button it names.
						for (const name of ['Take over', 'Sign in']) {
							await button(name).hover();
							await expect(tip(name)).toBeVisible();
							const file = await audited(`tooltip-${slug(name)}`, page, false);
							const t = (await page.evaluate(tooltipBox))!;
							const named = (await button(name).boundingBox())!;
							boxes[file] = { tip: t, button: named };
							const said = `${name}: ${JSON.stringify({ tip: t, button: named })}`;
							expect(t.text.startsWith(TIPS[name]), said).toBe(true);
							// A pixel at the edge, as for everything else on the page (see `audit`): a tooltip
							// is as wide as its text, which is no whole number of pixels (302.06), and it is
							// placed from a whole one.
							expect(
								t.left >= -1 &&
									t.top >= -1 &&
									t.right <= t.window.width + 1 &&
									t.bottom <= t.window.height + 1,
								`the tooltip is inside the window: ${said}`
							).toBe(true);
							expect(t.width, `the tooltip is no wider than 20rem: ${said}`).toBeLessThanOrEqual(
								320.5
							);
							expect(t.wider, `nothing is wider than the tooltip: ${said}`).toBeLessThanOrEqual(0);
							expect(
								t.top >= named.y + named.height - 1 || t.bottom <= named.y + 1,
								`the tooltip is beside its button, not on it: ${said}`
							).toBe(true);
						}
						await page.mouse.move(width - 2, 2);
						await expect(page.locator('[data-slot=tooltip-content]')).toHaveCount(0);

						// The ring of each button of the header, as Tab reaches it.
						await chooser.focus();
						for (const name of ['Take over', 'Sign in']) await tabTo(name);

						// Taking over the blank page: the address row, with the address of a blank page.
						await button('Take over').click();
						await expect(panel).toHaveAttribute('data-status', 'you');
						const field = panel.getByRole('textbox', { name: 'Address' });
						await expect(field).toHaveValue('about:blank');
						addressHolds(await capture('holding-blank'), 'holding-blank');

						// An address is typed and opened; a step back is the blank page, a step forward the
						// page that was left.
						await field.fill(address);
						await field.press('Enter');
						await expect(title).toHaveText(SITE_TITLE);
						await expect(field).toHaveValue(address);
						await button('Back').click();
						await expect(title).toHaveText('Blank page');
						await expect(field).toHaveValue('about:blank');
						await button('Forward').click();
						await expect(title).toHaveText(SITE_TITLE);
						await expect(field).toHaveValue(address);
						const holding = await capture('holding');
						addressHolds(holding, 'holding');
						expect(holding.buttons.map((b) => b.name)).toEqual(['Hand back', 'Sign in']);
						expect(
							Object.keys(holding.parts),
							'the header, the address, the picture, the note'
						).toEqual(['header', 'address', 'picture', 'note']);

						// The rings while holding: Hand back in the header, and the buttons of the address row.
						await chooser.focus();
						for (const name of ['Hand back', 'Back', 'Forward', 'Reload']) await tabTo(name);

						// What the host refuses is said under the header: there is no page to go forward to.
						await button('Forward').click();
						const alert = panel.locator('[data-browser-error]').getByRole('alert');
						await expect(alert).toContainText("there is no later page in this tab's history");
						const failed = await capture('holding-error');
						addressHolds(failed, 'holding-error');
						expect(
							Object.keys(failed.parts),
							'the header, the alert, the address, the picture, the note'
						).toEqual(['header', 'error', 'address', 'picture', 'note']);
						// The next command that works takes the alert away.
						await button('Reload').click();
						await expect(alert).toHaveCount(0);
						await button('Hand back').click();
						await expect(panel).toHaveAttribute('data-status', 'watching');

						// Another device takes the browser over: this one says so, and cannot take it.
						// The other device is a wide window whatever this one is: a context takes the
						// viewport of the test unless it is given one, and the zoomed one has no pane.
						const second = await browser.newContext({
							viewport: { width: 1440, height: 900 },
							deviceScaleFactor: 1
						});
						others.push(second);
						const holder = await second.newPage();
						await holder.goto(await stack.newLink());
						await expect(holder.locator('[data-session-list]:visible a').first()).toBeVisible();
						await open(holder, true);
						const theirs = panelOf(holder);
						await button('Take over', theirs).click();
						await expect(theirs).toHaveAttribute('data-status', 'you');
						await expect(panel).toHaveAttribute('data-status', 'other');
						await expect(panel.getByRole('status')).toHaveText('Held elsewhere');
						await expect(button('Take over')).toBeDisabled();
						await expect(panel.locator('[data-browser-note]')).toContainText(
							'Another device has taken the browser over'
						);
						const elsewhere = await capture('held-elsewhere');
						expect(elsewhere.buttons.map((b) => b.name)).toEqual(['Take over', 'Sign in']);
						expect(Object.keys(elsewhere.parts), 'the header, the picture, the note').toEqual([
							'header',
							'picture',
							'note'
						]);
						// The sign-in is refused while the other device holds it, and that is said here.
						await button('Sign in').click();
						await expect(alert).toContainText('another device has the browser');
						const refused = await capture('held-elsewhere-error');
						expect(
							Object.keys(refused.parts),
							'the header, the alert, the picture, the note'
						).toEqual(['header', 'error', 'picture', 'note']);
						await button('Hand back', theirs).click();
						await expect(panel).toHaveAttribute('data-status', 'watching');
					}

					// A view link may not watch: the panel says why, and offers nothing to press.
					const viewing = await browser.newContext({
						viewport: { width, height },
						deviceScaleFactor: view.scale
					});
					others.push(viewing);
					await viewing.addInitScript((t) => localStorage.setItem('azazel-theme', t), theme);
					const watcher = await viewing.newPage();
					await watcher.goto(await stack.newLink('v'));
					await expect(watcher.locator('[data-session-list]:visible a').first()).toBeVisible();
					await open(watcher, wide);
					await expect(panelOf(watcher).locator('[data-browser-denied]')).toContainText(
						'Watching needs a control link'
					);
					const file = await audited('view-link', watcher);
					const d = await watcher.evaluate(deniedBoxes);
					boxes[file] = d;
					const said = (what: string) => `view-link: ${what}: ${JSON.stringify(d)}`;
					expect(d.controls, said('nothing to press')).toBe(0);
					expect(d.sideways, said('nothing scrolls sideways')).toEqual({ page: 0, panel: 0 });
					for (const [name, part] of [
						['what is said', d.denied],
						['its title', d.title],
						['its text', d.text]
					] as const)
						expect(
							part.left >= d.panel.left - 0.5 &&
								part.right <= d.panel.right + 0.5 &&
								part.top >= d.panel.top - 0.5,
							said(`${name} is inside the panel`)
						).toBe(true);
					expect(d.title.bottom, said('the text is under its title')).toBeLessThanOrEqual(
						d.text.top + 0.5
					);
				} finally {
					writeFileSync(join(dir, 'measured.json'), JSON.stringify(measured, null, '\t'));
					writeFileSync(join(dir, 'boxes.json'), JSON.stringify(boxes, null, '\t'));
					for (const context of others) await context.close();
					await stack.stop();
					await site.close();
				}

				const report = [...where].map(([f, states]) => `${f}  [${states.join(', ')}]`).sort();
				expect(report.join('\n'), `${basename(dir)}: ${report.length} findings`).toBe('');
			}
		);
