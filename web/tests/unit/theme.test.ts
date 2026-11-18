import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import {
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
	SURFACES,
	TEXT_FLOOR,
	toHex,
	TOKENS,
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
	/** The oklch values of a custom property of app.css, as [light, dark]. */
	function fromCss(name: string): number[][] {
		const decl = new RegExp(`(?<![\\w-])--${name}:\\s*([^;]+);`).exec(css)?.[1] ?? '';
		const found = [...decl.matchAll(/oklch\(([^)]+)\)/g)].map((m) =>
			m[1]
				.split(/[\s/]+/)
				.filter(Boolean)
				.map(Number)
		);
		return found.length === 1 ? [found[0], found[0]] : found;
	}

	it('mirror the tokens of app.css', () => {
		for (const [name, pair] of Object.entries(TOKENS))
			expect(fromCss(name), `--${name}`).toEqual(pair);
	});
	it('keep every text pair at 4.5:1 and every control pair at 3:1, dark and light', () => {
		expect(PAIRS.length).toBeGreaterThan(100);
		expect(failures(builtin(false))).toEqual([]);
		expect(failures(builtin(true))).toEqual([]);
	});
	it('keep text and controls readable on a hovered or chosen row of every surface', () => {
		const bad: string[] = [];
		for (const light of [false, true]) {
			const p = builtin(light);
			for (const wash of ['hover', 'active']) {
				const [l, c, h, a] = TOKENS[wash][light ? 0 : 1] as [number, number, number, number];
				for (const s of [...SURFACES, 'composer', 'bubble']) {
					const under = composite(oklch(l, c, h), a, p[s]);
					const drawn: [string, number][] = [
						...PAIRS.filter(([, bg, f]) => bg === 'background' && f === TEXT_FLOOR).map(
							([t]): [string, number] => [t, TEXT_FLOOR]
						),
						['input', CONTROL_FLOOR],
						['ring', CONTROL_FLOOR]
					];
					for (const [fg, floor] of drawn) {
						const ratio = contrast(p[fg], under);
						if (ratio < floor)
							bad.push(
								`${light ? 'light' : 'dark'} ${fg} on ${wash} over ${s}: ${ratio.toFixed(2)}`
							);
					}
				}
			}
		}
		expect(bad).toEqual([]);
	});
	it('draw the edge of the message box and of a segmented choice at 3:1 on what is around them', () => {
		// Both edges are --input; the rule below pins that, the pairs pin its ratio on every surface
		// they sit on (the track of a .seg is --inset, the box itself is --composer).
		const rule = (sel: string) => {
			const at = css.indexOf(`\n\t${sel} {`);
			return css.slice(at, css.indexOf('\n\t}', at));
		};
		expect(rule('.composer')).toMatch(/border: 1px solid var\(--input\)/);
		expect(rule('.seg')).toMatch(/inset 0 0 0 1px var\(--input\)/);
		const edge = (bg: string) =>
			PAIRS.some(([fg, b, f]) => fg === 'input' && b === bg && f === CONTROL_FLOOR);
		for (const bg of ['background', 'rail', 'card', 'overlay', 'inset', 'composer'])
			expect(edge(bg), bg).toBe(true);
		for (const light of [false, true])
			for (const bg of ['background', 'rail', 'card', 'overlay', 'inset', 'composer'])
				expect(
					contrast(builtin(light).input, builtin(light)[bg]),
					`${light} ${bg}`
				).toBeGreaterThanOrEqual(3);
	});
	it('keep those edges at 3:1 when a terminal theme is matched', () => {
		for (const t of [TOKYO, PAPER]) {
			const { palette } = mapTheme(t);
			for (const bg of ['background', 'rail', 'card', 'overlay', 'inset', 'composer'])
				expect(contrast(palette.input, palette[bg]), `${t.name} ${bg}`).toBeGreaterThanOrEqual(3);
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
		expect(m.vars['--bubble']).toBe('#292e42');
		expect(m.vars['--inset']).toBe('#16161e');
		expect(m.vars['--active']).toBe('#283457');
		expect(failures(m.palette)).toEqual([]);
	});
	it('only sets variables that leaving the theme clears again', () => {
		const m = mapTheme(TOKYO);
		expect(Object.keys(m.vars).filter((k) => !MAPPED_VARS.includes(k))).toEqual([]);
	});
	it('falls back to the built-in token for colours too pale to read', () => {
		const m = mapTheme(PAPER);
		expect(m.light).toBe(true);
		expect(m.fellBack).toContain('ember-text');
		expect(m.fellBack).toContain('destructive');
		expect(m.vars['--ember-text']).toBeUndefined();
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
