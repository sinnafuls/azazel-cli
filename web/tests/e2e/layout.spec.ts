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
// Screenshots and the measurements of every state go to tmp/layout, or to AZAZEL_LAYOUT_SHOTS;
// those of the profiles (their chip, menu, settings and the notice of a usage limit) to a folder of
// their own beside each size's, `<size>-<theme>-profiles`.
import { mkdirSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { test, expect, Stack, type Page } from './stack';

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
				// A warning that comes while the error is still there: the two, one above the other.
				await box.fill('/name ');
				await box.press('Enter');
				await expect(toast).toHaveCount(2);
				await expect(toast.first()).toHaveAttribute('data-type', 'warning');
				await expect(toast.first()).toHaveText(/\/name needs <title>\./);
				await toast.first().hover();
				await expect(toast.last()).toHaveAttribute('data-expanded', 'true');
				await capture('toast-stack');
				await closeToast.last().click();
				await expect(toast).toHaveCount(1);
				await page.mouse.move(width - 2, 2);
				await capture('toast-warning');
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
				// Newest first: the two from the page, the note of the done check, the first prompt.
				await expect(prompts).toHaveCount(4);
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
			} finally {
				writeFileSync(join(dir, 'measured.json'), JSON.stringify(measured, null, '\t'));
				await stack.stop();
			}

			const report = [...where].map(([f, states]) => `${f}  [${states.join(', ')}]`).sort();
			expect(report.join('\n'), `${basename(dir)}: ${report.length} findings`).toBe('');
		});
