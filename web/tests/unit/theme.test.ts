import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import {
	ACCENT_VARS,
	ACCENTS,
	accentVars,
	builtin,
	composite,
	contrast,
	CONTROL_FLOOR,
	failures,
	mapTheme,
	MAPPED_VARS,
	oklch,
	PAIRS,
	parseHex,
	preview,
	PREVIEW_TOKENS,
	SURFACES,
	TEXT_FLOOR,
	toHex,
	TOKENS,
	withAccent,
	type Rgb,
	type ThemeInput
} from '$lib/theme';

const hex = (h: string) => parseHex(h) as Rgb;

const TOKYO: ThemeInput = {
	name: 'tokyo-night',
	light: false,
	roles: {
		text: '#c0caf5',
		muted: '#9aa5ce',
		accent: '#7aa2f7',
		success: '#9ece6a',
		warning: '#e0af68',
		error: '#f7768e',
		border: '#7a84b0',
		code_bg: '#16161e',
		diff_add: '#9ece6a',
		diff_del: '#f7768e',
		user_bg: '#292e42',
		user_text: '#c0caf5',
		selected_bg: '#283457',
		border_muted: '#3b4261',
		diff_add_bg: '#20303b',
		diff_del_bg: '#37222c',
		thinking: '#9aa5ce',
		bash: '#bb9af7',
		mode_plan: '#7dcfff',
		mode_edits: '#9ece6a',
		mode_auto: '#e0af68',
		surface: '#1a1b26',
		backdrop: '#16161e'
	}
};
// A light theme whose warning and error are too pale to read on its paper.
const PAPER: ThemeInput = {
	name: 'paper',
	light: true,
	roles: {
		text: '#2e3440',
		muted: '#5c6370',
		accent: '#2f5fb3',
		success: '#2f7d32',
		warning: '#e8c06a',
		error: '#ff9aa2',
		border: '#8a8f98',
		code_bg: '#f2efe8',
		user_bg: '#ece8df',
		user_text: '#2e3440',
		surface: '#faf8f3'
	}
};

describe('colour maths', () => {
	it('reads and writes #rrggbb and refuses anything else', () => {
		expect(parseHex('#1a1b26')).toEqual([26, 27, 38]);
		expect(toHex([26, 27, 38])).toBe('#1a1b26');
		expect(parseHex('red')).toBeNull();
		expect(parseHex('#fff')).toBeNull();
		expect(parseHex(undefined)).toBeNull();
	});
	it('measures contrast as WCAG does', () => {
		expect(contrast(hex('#000000'), hex('#ffffff'))).toBeCloseTo(21, 5);
		expect(contrast(hex('#777777'), hex('#777777'))).toBeCloseTo(1, 5);
		expect(contrast(hex('#ffffff'), hex('#000000'))).toBeCloseTo(21, 5);
	});
});

describe('the built-in themes', () => {
	const css = readFileSync(resolve(process.cwd(), 'src', 'app.css'), 'utf8');
	/** The oklch values of a custom property of app.css, as [light (`:root`), dark (`.dark`)]. */
	function fromCss(name: string): number[][] {
		const found = [
			...css.matchAll(new RegExp(`(?<![\\w-])--${name}:\\s*oklch\\(([^)]+)\\);`, 'g'))
		].map((m) =>
			m[1]
				.split(/[\s/]+/)
				.filter(Boolean)
				.map((v) => (v.endsWith('%') ? Number(v.slice(0, -1)) / 100 : Number(v)))
		);
		return found.length === 1 ? [found[0], found[0]] : found;
	}

	it('mirror the tokens of app.css', () => {
		for (const [name, pair] of Object.entries(TOKENS))
			expect(fromCss(name), `--${name}`).toEqual(pair);
	});
	it('are the tokens of sin-api: neutral greys, one emerald accent, alpha hairlines', () => {
		const p = builtin(false);
		expect(toHex(p.background)).toBe(toHex(oklch(0.145, 0, 0)));
		expect(toHex(p.card)).toBe(toHex(oklch(0.17, 0, 0)));
		expect(toHex(p.sidebar)).toBe(toHex(oklch(0.16, 0, 0)));
		expect(TOKENS.border).toEqual([
			[0, 0, 0, 0.09],
			[1, 0, 0, 0.09]
		]);
		expect(TOKENS.input).toEqual([
			[0, 0, 0, 0.12],
			[1, 0, 0, 0.12]
		]);
		expect(TOKENS.ring).toEqual(TOKENS.brand);
		for (const s of SURFACES) for (const v of TOKENS[s]) expect(v[1], s).toBe(0);
	});
	it('keep every text pair at 4.5:1 and every control pair at 3:1, dark and light', () => {
		expect(TEXT_FLOOR).toBe(4.5);
		expect(CONTROL_FLOOR).toBe(3);
		expect(PAIRS.length).toBeGreaterThan(70);
		expect(failures(builtin(false))).toEqual([]);
		expect(failures(builtin(true))).toEqual([]);
	});
	it('keep text readable on a hovered or chosen row of every surface', () => {
		// A wash is an opaque step of the ladder here, so it is a surface like any other.
		for (const wash of ['muted', 'accent', 'sidebar-accent'])
			for (const t of ['foreground', 'muted-foreground', 'success', 'warning'])
				expect(
					PAIRS.some(([fg, bg, f]) => fg === t && bg === wash && f === TEXT_FLOOR),
					`${t} on ${wash}`
				).toBe(true);
	});
	it('keep text readable on the fill of a field and of a code block', () => {
		// `dark:bg-input/30`: the hairline colour at 30% over the surface the control sits on.
		const bad: string[] = [];
		for (const light of [false, true]) {
			const p = builtin(light);
			const [l, c, h, a] = TOKENS.input[light ? 0 : 1] as [number, number, number, number];
			for (const s of ['background', 'card', 'popover', 'sidebar']) {
				const under = composite(oklch(l, c, h), a * 0.3, p[s]);
				for (const t of [
					'foreground',
					'muted-foreground',
					'syn-text',
					'syn-key',
					'syn-string',
					'syn-number',
					'syn-keyword',
					'syn-fn'
				]) {
					const ratio = contrast(p[t], under);
					if (ratio < TEXT_FLOOR)
						bad.push(`${light ? 'light' : 'dark'} ${t} on input/30 over ${s}: ${ratio.toFixed(2)}`);
				}
			}
		}
		expect(bad).toEqual([]);
	});
	it('draw the focus ring and a filled button at 3:1 on what is around them', () => {
		// The edge of a field at rest is sin-api's 12% hairline, which is quieter than 3:1 by design;
		// what marks a control is its fill, its label and, with the keyboard, this ring.
		for (const light of [false, true])
			for (const bg of ['background', 'card', 'popover', 'sidebar'])
				for (const fg of ['ring', 'primary'])
					expect(
						contrast(builtin(light)[fg], builtin(light)[bg]),
						`${light ? 'light' : 'dark'} ${fg} on ${bg}`
					).toBeGreaterThanOrEqual(CONTROL_FLOOR);
	});
	it('keep the ring at 3:1 when a terminal theme is matched', () => {
		for (const t of [TOKYO, PAPER]) {
			const { palette } = mapTheme(t);
			for (const bg of ['background', 'card', 'popover', 'sidebar'])
				expect(contrast(palette.ring, palette[bg]), `${t.name} ${bg}`).toBeGreaterThanOrEqual(
					CONTROL_FLOOR
				);
		}
	});
	it('notice a pair that is too faint', () => {
		const bad = failures({ ...builtin(false), foreground: hex('#333333') });
		expect(bad.some((f) => f.fg === 'foreground' && f.floor === 4.5)).toBe(true);
	});
});

describe('mapping a terminal theme', () => {
	it('sets nothing without a theme', () => {
		expect(mapTheme(null).vars).toEqual({});
		expect(mapTheme(undefined).fellBack).toEqual([]);
	});
	it('takes a readable theme as it is', () => {
		const m = mapTheme(TOKYO);
		expect(m.light).toBe(false);
		expect(m.fellBack).toEqual([]);
		expect(m.vars['--background']).toBe('#1a1b26');
		expect(m.vars['--foreground']).toBe('#c0caf5');
		expect(m.vars['--sidebar-foreground']).toBe('#c0caf5');
		expect(m.vars['--secondary']).toBe('#292e42');
		expect(m.vars['--accent']).toBe('#283457');
		expect(m.vars['--ring']).toBe('#7aa2f7');
		expect(failures(m.palette)).toEqual([]);
	});
	it('only sets variables that leaving the theme clears again', () => {
		const m = mapTheme(TOKYO);
		expect(Object.keys(m.vars).filter((k) => !MAPPED_VARS.includes(k))).toEqual([]);
	});
	it('falls back to the built-in token for colours too pale to read', () => {
		const m = mapTheme(PAPER);
		expect(m.light).toBe(true);
		expect(m.fellBack).toContain('warning');
		expect(m.fellBack).toContain('destructive');
		expect(m.vars['--warning']).toBeUndefined();
		expect(m.vars['--destructive']).toBeUndefined();
		expect(failures(m.palette)).toEqual([]);
	});
	it('never leaves text unreadable, whatever the terminal sends', () => {
		const m = mapTheme({
			name: 'broken',
			light: false,
			roles: { ...TOKYO.roles, text: '#2a2b36', muted: '#20212c' }
		});
		expect(m.fellBack).toContain('foreground');
		expect(m.vars['--foreground']).toBeUndefined();
		expect(failures(m.palette)).toEqual([]);
	});
	it('ignores values that are not colours', () => {
		const m = mapTheme({ name: 'odd', light: false, roles: { text: 'red', surface: 'nope' } });
		expect(m.vars).toEqual({});
		expect(m.fellBack).toEqual([]);
	});
});

describe('the accents', () => {
	// Where PAIRS holds a ring and the accent to the control floor: the page, a card, a dialog, the
	// sidebar, and the track of a meter.
	const GROUNDS = ['background', 'card', 'popover', 'sidebar', 'muted'];
	// Emerald is sin-api's token, kept value for value: on the light theme it is 2.9:1 on a chosen
	// sidebar row and a little outside sRGB. The accents this page adds are held to more.
	const ADDED = ACCENTS.filter((a) => a.value !== 'emerald');

	it('offers emerald first, as the stylesheet writes it', () => {
		expect(ACCENTS[0].value).toBe('emerald');
		expect(accentVars('emerald', true)).toEqual({});
		expect(accentVars('emerald', false)).toEqual({});
		expect(new Set(ACCENTS.map((a) => a.value)).size).toBe(ACCENTS.length);
	});

	for (const a of ACCENTS)
		for (const light of [true, false]) {
			const side = light ? 'light' : 'dark';
			it(`${a.value} reaches ${CONTROL_FLOOR}:1 where a control is drawn (${side})`, () => {
				const p = withAccent(a.value, light);
				for (const ground of GROUNDS)
					expect(
						contrast(p.brand, p[ground]),
						`${a.value} on ${ground} (${side})`
					).toBeGreaterThanOrEqual(CONTROL_FLOOR);
				expect(p.ring).toEqual(p.brand);
				// The theme still holds as a whole with this accent in it.
				expect(failures(p)).toEqual([]);
			});
		}

	for (const a of ADDED)
		for (const light of [true, false]) {
			const side = light ? 'light' : 'dark';
			it(`${a.value} reaches ${CONTROL_FLOOR}:1 on a hovered or chosen row too (${side})`, () => {
				const p = withAccent(a.value, light);
				for (const ground of SURFACES)
					expect(
						contrast(p.brand, p[ground]),
						`${a.value} on ${ground} (${side})`
					).toBeGreaterThanOrEqual(CONTROL_FLOOR);
			});
			it(`${a.value} is a colour a screen can show (${side})`, () => {
				// In gamut: nothing is clipped on the way to sRGB, so what is checked is what is drawn.
				const [l, c, h] = a.tone[light ? 0 : 1];
				const rad = (h * Math.PI) / 180;
				const A = c * Math.cos(rad);
				const B = c * Math.sin(rad);
				const L = (l + 0.3963377774 * A + 0.2158037573 * B) ** 3;
				const M = (l - 0.1055613458 * A - 0.0638541728 * B) ** 3;
				const S = (l - 0.0894841775 * A - 1.291485548 * B) ** 3;
				for (const v of [
					4.0767416621 * L - 3.3077115913 * M + 0.2309699292 * S,
					-1.2684380046 * L + 2.6097574011 * M - 0.3413193965 * S,
					-0.0041960863 * L - 0.7034186147 * M + 1.707614701 * S
				]) {
					expect(v).toBeGreaterThanOrEqual(-0.002);
					expect(v).toBeLessThanOrEqual(1.002);
				}
			});
		}

	it('sets the accent and both focus rings, and only variables that leaving a theme clears', () => {
		const vars = accentVars('violet', false);
		expect(Object.keys(vars).sort()).toEqual(ACCENT_VARS.map((v) => `--${v}`).sort());
		expect(new Set(Object.values(vars)).size).toBe(1);
		for (const v of Object.keys(vars)) expect(MAPPED_VARS).toContain(v);
		expect(accentVars('violet', true)['--brand']).not.toBe(vars['--brand']);
	});

	it('has the first chart colour follow the accent in the stylesheet', () => {
		const sheet = readFileSync(resolve(__dirname, '../../src/app.css'), 'utf8');
		expect(sheet).toMatch(/--chart-1:\s*var\(--brand\)/);
	});

	it("draws a theme card with that theme's colours and the accent in use", () => {
		const dark = preview(false, 'rose');
		const light = preview(true, 'rose');
		for (const t of PREVIEW_TOKENS) expect(dark[t]).toMatch(/^oklch\(/);
		expect(dark.background).not.toBe(light.background);
		expect(dark.brand).toBe(accentVars('rose', false)['--brand']);
		expect(light.brand).toBe(accentVars('rose', true)['--brand']);
		// A matched terminal theme is drawn in its own colours, its accent among them.
		const mapped = mapTheme(TOKYO);
		const term = preview(false, 'rose', mapped);
		expect(term.background).toBe(mapped.vars['--background']);
		expect(term.brand).toBe(mapped.vars['--brand']);
		expect(term.brand).not.toBe(dark.brand);
	});
});
