// "Match terminal": the terminal's theme roles mapped onto the page's CSS variables.
//
// Pure: colours in, a table of CSS variables out. A terminal theme is somebody else's palette, so
// every pair the page relies on is measured (WCAG 2 contrast) and whatever falls under its floor
// goes back to the built-in token; a theme can restyle the page but never make it unreadable.

export interface ThemeInput {
	name: string;
	light: boolean;
	roles: Record<string, string>;
}

export type Rgb = [number, number, number];

// --- colour maths ----------------------------------------------------------------------------

export function parseHex(hex: string | undefined | null): Rgb | null {
	const m = /^#?([0-9a-f]{6})$/i.exec((hex ?? '').trim());
	if (!m) return null;
	const n = parseInt(m[1], 16);
	return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

export function toHex([r, g, b]: Rgb): string {
	const h = (v: number) =>
		Math.max(0, Math.min(255, Math.round(v)))
			.toString(16)
			.padStart(2, '0');
	return `#${h(r)}${h(g)}${h(b)}`;
}

const toLinear = (v: number) => {
	const c = v / 255;
	return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};
const fromLinear = (c: number) =>
	255 * (c <= 0.0031308 ? 12.92 * c : 1.055 * Math.max(c, 0) ** (1 / 2.4) - 0.055);

/** Relative luminance, WCAG 2. */
export function luminance(rgb: Rgb): number {
	const [r, g, b] = rgb.map(toLinear);
	return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** Contrast ratio of two opaque colours, 1 to 21. */
export function contrast(a: Rgb, b: Rgb): number {
	const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
	return (hi + 0.05) / (lo + 0.05);
}

/** `over` at `alpha` painted on `under`. */
export function composite(over: Rgb, alpha: number, under: Rgb): Rgb {
	return [0, 1, 2].map((i) => over[i] * alpha + under[i] * (1 - alpha)) as Rgb;
}

/** OKLCH (as CSS writes it: lightness 0..1, chroma, hue in degrees) to sRGB, clipped to gamut. */
export function oklch(l: number, c: number, h: number): Rgb {
	const rad = (h * Math.PI) / 180;
	return oklab(l, c * Math.cos(rad), c * Math.sin(rad));
}
function oklab(L: number, a: number, b: number): Rgb {
	const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
	const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
	const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
	return [
		fromLinear(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
		fromLinear(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
		fromLinear(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s)
	].map((v) => Math.max(0, Math.min(255, v))) as Rgb;
}
function toOklab(rgb: Rgb): [number, number, number] {
	const [r, g, b] = rgb.map(toLinear);
	const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
	const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
	const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
	return [
		0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
		1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
		0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s
	];
}
/** The colour with its lightness moved by `delta` (OKLab), keeping its hue. */
export function shift(rgb: Rgb, delta: number): Rgb {
	const [L, a, b] = toOklab(rgb);
	return oklab(Math.max(0, Math.min(1, L + delta)), a, b);
}
export function mix(a: Rgb, b: Rgb, t: number): Rgb {
	return composite(b, t, a);
}

// --- the built-in tokens -----------------------------------------------------------------------

/** `[l, c, h]` or `[l, c, h, alpha]`, as `src/app.css` writes them. */
export type Lch = [number, number, number] | [number, number, number, number];

/**
 * The opaque and alpha tokens of `src/app.css` that the contrast check needs, as [light, dark].
 * tests/unit/theme.test.ts reads app.css and fails when these and the stylesheet differ.
 */
export const TOKENS: Record<string, [Lch, Lch]> = {
	rail: [
		[0.97, 0, 0],
		[0.16, 0, 0]
	],
	background: [
		[1, 0, 0],
		[0.2, 0, 0]
	],
	card: [
		[0.98, 0, 0],
		[0.235, 0, 0]
	],
	raised: [
		[0.945, 0, 0],
		[0.27, 0, 0]
	],
	overlay: [
		[1, 0, 0],
		[0.27, 0, 0]
	],
	inset: [
		[0.965, 0, 0],
		[0.165, 0, 0]
	],
	field: [
		[1, 0, 0],
		[0.18, 0, 0]
	],
	composer: [
		[1, 0, 0],
		[0.235, 0, 0]
	],
	bubble: [
		[0.945, 0, 0],
		[0.27, 0, 0]
	],
	'bubble-foreground': [
		[0.2, 0, 0],
		[0.95, 0, 0]
	],
	hover: [
		[0, 0, 0, 0.05],
		[1, 0, 0, 0.06]
	],
	active: [
		[0, 0, 0, 0.08],
		[1, 0, 0, 0.1]
	],
	input: [
		[0.58, 0, 0],
		[0.63, 0, 0]
	],
	foreground: [
		[0.2, 0, 0],
		[0.95, 0, 0]
	],
	'ink-2': [
		[0.33, 0, 0],
		[0.86, 0, 0]
	],
	'muted-foreground': [
		[0.43, 0, 0],
		[0.77, 0, 0]
	],
	faint: [
		[0.47, 0, 0],
		[0.74, 0, 0]
	],
	primary: [
		[0.2, 0, 0],
		[0.96, 0, 0]
	],
	'primary-hover': [
		[0.32, 0, 0],
		[0.86, 0, 0]
	],
	'primary-foreground': [
		[0.985, 0, 0],
		[0.18, 0, 0]
	],
	'accent-text': [
		[0.45, 0.16, 255],
		[0.8, 0.1, 250]
	],
	ring: [
		[0.5, 0.18, 255],
		[0.74, 0.13, 250]
	],
	ember: [
		[0.62, 0.14, 60],
		[0.8, 0.13, 75]
	],
	'ember-foreground': [
		[0.2, 0.04, 60],
		[0.2, 0.04, 60]
	],
	'ember-text': [
		[0.45, 0.1, 60],
		[0.84, 0.11, 80]
	],
	info: [
		[0.46, 0.16, 255],
		[0.78, 0.11, 250]
	],
	success: [
		[0.45, 0.12, 155],
		[0.8, 0.14, 155]
	],
	destructive: [
		[0.48, 0.19, 25],
		[0.76, 0.14, 22]
	],
	'destructive-foreground': [
		[0.99, 0, 0],
		[0.16, 0.03, 22]
	],
	alarm: [
		[0.5, 0.2, 27],
		[0.5, 0.2, 27]
	],
	'alarm-foreground': [
		[0.99, 0, 0],
		[0.99, 0, 0]
	],
	'diff-add': [
		[0.955, 0.04, 155],
		[0.27, 0.04, 155]
	],
	'diff-del': [
		[0.96, 0.028, 22],
		[0.27, 0.045, 22]
	],
	'diff-add-fg': [
		[0.38, 0.11, 155],
		[0.86, 0.11, 155]
	],
	'diff-del-fg': [
		[0.44, 0.15, 22],
		[0.86, 0.09, 22]
	],
	'syn-key': [
		[0.45, 0.1, 300],
		[0.82, 0.09, 300]
	]
};

/** Tokens that only point at another one. */
const ALIASES: Record<string, string> = {
	thinking: 'muted-foreground',
	bash: 'ink-2',
	'mode-plan': 'info',
	'mode-edits': 'success',
	'mode-auto': 'ember-text'
};

export type Palette = Record<string, Rgb>;

/** The built-in theme as opaque colours; the alpha washes are left out (see `TOKENS`). */
export function builtin(light: boolean): Palette {
	const out: Palette = {};
	for (const [name, pair] of Object.entries(TOKENS)) {
		const v = pair[light ? 0 : 1];
		if (v.length === 3) out[name] = oklch(v[0], v[1], v[2]);
	}
	for (const [name, of] of Object.entries(ALIASES)) out[name] = out[of];
	return out;
}

// --- what has to be readable -------------------------------------------------------------------

export const TEXT_FLOOR = 4.5;
export const CONTROL_FLOOR = 3;

/** The surfaces running text and glyphs sit on. */
export const SURFACES = ['background', 'rail', 'card', 'raised', 'overlay', 'inset', 'field'];

/** Every pair the page relies on: `[what is drawn, what it is drawn on, the floor]`. */
export const PAIRS: [string, string, number][] = [
	...[
		'foreground',
		'ink-2',
		'muted-foreground',
		'faint',
		'accent-text',
		'info',
		'success',
		'destructive',
		'ember-text',
		'thinking',
		'bash',
		'mode-plan',
		'mode-edits',
		'mode-auto',
		'syn-key'
	].flatMap((t) => SURFACES.map((s): [string, string, number] => [t, s, TEXT_FLOOR])),
	['foreground', 'composer', TEXT_FLOOR],
	['faint', 'composer', TEXT_FLOOR],
	['bubble-foreground', 'bubble', TEXT_FLOOR],
	['primary-foreground', 'primary', TEXT_FLOOR],
	['primary-foreground', 'primary-hover', TEXT_FLOOR],
	['destructive-foreground', 'destructive', TEXT_FLOOR],
	['ember-foreground', 'ember', TEXT_FLOOR],
	['alarm-foreground', 'alarm', TEXT_FLOOR],
	['diff-add-fg', 'diff-add', TEXT_FLOOR],
	['diff-del-fg', 'diff-del', TEXT_FLOOR],
	['foreground', 'diff-add', TEXT_FLOOR],
	['foreground', 'diff-del', TEXT_FLOOR],
	// controls: an edge, a filled button against the page, the focus ring, a status mark
	...['background', 'rail', 'card', 'overlay', 'inset', 'field', 'composer'].map(
		(s): [string, string, number] => ['input', s, CONTROL_FLOOR]
	),
	...['background', 'rail', 'card', 'overlay', 'composer'].map((s): [string, string, number] => [
		'ring',
		s,
		CONTROL_FLOOR
	]),
	...['background', 'rail', 'card', 'overlay'].map((s): [string, string, number] => [
		'primary',
		s,
		CONTROL_FLOOR
	]),
	...['background', 'rail', 'overlay'].flatMap((s) =>
		['ember', 'info', 'success', 'destructive'].map((m): [string, string, number] => [
			m,
			s,
			CONTROL_FLOOR
		])
	)
];

/** The pairs of `p` under their floor. */
export function failures(p: Palette): { fg: string; bg: string; ratio: number; floor: number }[] {
	const out = [];
	for (const [fg, bg, floor] of PAIRS) {
		const ratio = contrast(p[fg], p[bg]);
		if (ratio < floor) out.push({ fg, bg, ratio, floor });
	}
	return out;
}

// --- the mapping ------------------------------------------------------------------------------

/** How far each surface sits from the canvas in lightness (OKLab), [light, dark]. */
const LADDER: Record<string, [number, number]> = {
	rail: [-0.03, -0.04],
	card: [-0.02, 0.035],
	raised: [-0.055, 0.07],
	overlay: [0, 0.07],
	field: [0, -0.02],
	composer: [0, 0.035]
};

/** Terminal role -> the variables it sets. */
const ROLE_VARS: Record<string, string[]> = {
	text: ['foreground'],
	muted: ['muted-foreground', 'faint'],
	accent: ['accent-text', 'ring'],
	success: ['success'],
	warning: ['ember', 'ember-text'],
	error: ['destructive'],
	border: ['input'],
	code_bg: ['inset'],
	diff_add: ['diff-add-fg'],
	diff_del: ['diff-del-fg'],
	diff_add_bg: ['diff-add'],
	diff_del_bg: ['diff-del'],
	user_bg: ['bubble'],
	user_text: ['bubble-foreground'],
	thinking: ['thinking'],
	bash: ['bash'],
	mode_plan: ['mode-plan', 'info'],
	mode_edits: ['mode-edits'],
	mode_auto: ['mode-auto']
};

export interface Mapped {
	/** CSS custom properties to set on the root, `--name: value`. */
	vars: Record<string, string>;
	/** The variables that kept the built-in value because the terminal's colour was under a floor. */
	fellBack: string[];
	/** Whether the built-in light or dark theme is the base. */
	light: boolean;
	/** The palette the page ends up with (opaque tokens only), for checking and for tests. */
	palette: Palette;
}

/**
 * Maps a terminal theme onto the page. Every variable the terminal sets is kept only when all the
 * pairs it takes part in still reach their floor; otherwise it goes back to the built-in token.
 * The result never has a pair of `PAIRS` under its floor: when nothing else works, the theme is
 * not applied at all.
 */
export function mapTheme(theme: ThemeInput | null | undefined): Mapped {
	const light = !!theme?.light;
	const base = builtin(light);
	if (!theme) return { vars: {}, fellBack: [], light, palette: base };

	// What the terminal asks for.
	const wanted: Palette = {};
	const surface = parseHex(theme.roles.surface);
	if (surface) {
		wanted.background = surface;
		for (const [name, d] of Object.entries(LADDER)) wanted[name] = shift(surface, d[light ? 0 : 1]);
	}
	for (const [role, vars] of Object.entries(ROLE_VARS)) {
		const c = parseHex(theme.roles[role]);
		if (c) for (const v of vars) wanted[v] = c;
	}
	const text = wanted.foreground;
	if (text && wanted.background) {
		wanted['ink-2'] = mix(text, wanted.background, 0.14);
		wanted.primary = text;
		wanted['primary-hover'] = mix(text, wanted.background, 0.16);
		wanted['primary-foreground'] = wanted.background;
	}

	// Start from everything asked for, then give up one variable at a time until every pair holds:
	// of a failing pair, the side drawn (the text, the edge) goes first, its surface second.
	const p: Palette = { ...base, ...wanted };
	const fellBack = new Set<string>();
	const revert = (name: string) => {
		if (!(name in wanted) || fellBack.has(name)) return false;
		p[name] = base[name];
		fellBack.add(name);
		return true;
	};
	for (let guard = 0; guard < 200; guard++) {
		const bad = failures(p);
		if (!bad.length) break;
		const f = bad[0];
		if (!revert(f.fg) && !revert(f.bg)) break;
	}
	if (failures(p).length) return { vars: {}, fellBack: Object.keys(wanted), light, palette: base };

	const vars: Record<string, string> = {};
	for (const name of Object.keys(wanted))
		if (!fellBack.has(name)) vars[`--${name}`] = toHex(p[name]);

	// Washes and hairlines have no floor of their own: they follow the terminal when it names them.
	const selected = parseHex(theme.roles.selected_bg);
	const fg = p.foreground;
	if (selected && SURFACES.every((s) => contrast(fg, selected) >= TEXT_FLOOR || !p[s])) {
		if (contrast(fg, selected) >= TEXT_FLOOR && contrast(p['muted-foreground'], selected) >= 4.5) {
			vars['--active'] = toHex(selected);
			vars['--hover'] = `${toHex(selected)}99`;
		}
	}
	const hairline = parseHex(theme.roles.border_muted);
	if (hairline) {
		vars['--border-strong'] = `${toHex(hairline)}cc`;
		vars['--border'] = `${toHex(hairline)}73`;
	}
	const backdrop = parseHex(theme.roles.backdrop);
	if (backdrop) vars['--scrim'] = `${toHex(shift(backdrop, light ? 0 : -0.25))}99`;

	return { vars, fellBack: [...fellBack].sort(), light, palette: p };
}

/** Every variable `mapTheme` may set, so that leaving "Match terminal" can clear them. */
export const MAPPED_VARS: string[] = [
	...new Set([
		'background',
		...Object.keys(LADDER),
		...Object.values(ROLE_VARS).flat(),
		'ink-2',
		'primary',
		'primary-hover',
		'primary-foreground',
		'active',
		'hover',
		'border',
		'border-strong',
		'scrim'
	])
].map((n) => `--${n}`);
