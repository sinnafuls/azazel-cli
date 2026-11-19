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

const grey = (light: number, dark: number): [Lch, Lch] => [
	[light, 0, 0],
	[dark, 0, 0]
];

/**
 * The tokens of `src/app.css` that the contrast check needs, as [light (`:root`), dark (`.dark`)].
 * They are sin-api's, plus `success` and `warning`. tests/unit/theme.test.ts reads app.css and
 * fails when these and the stylesheet differ.
 */
export const TOKENS: Record<string, [Lch, Lch]> = {
	background: grey(0.985, 0.145),
	card: grey(1, 0.17),
	popover: grey(1, 0.17),
	sidebar: grey(0.97, 0.16),
	muted: grey(0.96, 0.21),
	secondary: grey(0.96, 0.21),
	accent: grey(0.94, 0.24),
	'sidebar-accent': grey(0.93, 0.22),
	foreground: grey(0.145, 0.96),
	'secondary-foreground': grey(0.145, 0.96),
	'muted-foreground': grey(0.5, 0.66),
	primary: grey(0.145, 0.96),
	'primary-foreground': grey(0.985, 0.145),
	border: [
		[0, 0, 0, 0.09],
		[1, 0, 0, 0.09]
	],
	input: [
		[0, 0, 0, 0.12],
		[1, 0, 0, 0.12]
	],
	brand: [
		[0.6, 0.15, 162],
		[0.7, 0.15, 162]
	],
	ring: [
		[0.6, 0.15, 162],
		[0.7, 0.15, 162]
	],
	success: [
		[0.5, 0.12, 162],
		[0.7, 0.15, 162]
	],
	warning: [
		[0.5, 0.11, 70],
		[0.8, 0.13, 80]
	],
	destructive: [
		[0.58, 0.22, 27],
		[0.64, 0.21, 25]
	],
	'syn-text': grey(0.3, 0.86),
	'syn-key': [
		[0.5, 0.12, 162],
		[0.78, 0.12, 162]
	],
	'syn-string': [
		[0.5, 0.09, 70],
		[0.84, 0.08, 85]
	],
	'syn-number': [
		[0.54, 0.14, 40],
		[0.8, 0.11, 50]
	],
	'syn-keyword': [
		[0.5, 0.13, 255],
		[0.78, 0.09, 250]
	],
	'syn-fn': [
		[0.46, 0.1, 220],
		[0.84, 0.07, 210]
	]
};

export type Palette = Record<string, Rgb>;

/** The built-in theme as opaque colours; the alpha hairlines are left out (see `TOKENS`). */
export function builtin(light: boolean): Palette {
	const out: Palette = {};
	for (const [name, pair] of Object.entries(TOKENS)) {
		const v = pair[light ? 0 : 1];
		if (v.length === 3) out[name] = oklch(v[0], v[1], v[2]);
	}
	return out;
}

// --- the accent --------------------------------------------------------------------------------

/**
 * The accents Settings > General offers, as [light, dark]. Each is drawn as a focus ring, a meter
 * fill and the mark of what is running, so each reaches 3:1 on every surface those sit on
 * (tests/unit/theme.test.ts). Emerald is the one `src/app.css` writes out; the others are set on
 * the root over it.
 */
export const ACCENTS = [
	{ value: 'emerald', label: 'Emerald', tone: TOKENS.brand },
	{
		value: 'blue',
		label: 'Blue',
		tone: [
			[0.56, 0.19, 258],
			[0.7, 0.15, 255]
		]
	},
	{
		value: 'violet',
		label: 'Violet',
		tone: [
			[0.56, 0.21, 295],
			[0.72, 0.15, 295]
		]
	},
	{
		value: 'rose',
		label: 'Rose',
		tone: [
			[0.58, 0.2, 12],
			[0.72, 0.16, 12]
		]
	},
	{
		value: 'orange',
		label: 'Orange',
		tone: [
			[0.6, 0.16, 45],
			[0.75, 0.15, 55]
		]
	},
	{
		value: 'amber',
		label: 'Amber',
		tone: [
			[0.58, 0.12, 78],
			[0.8, 0.14, 85]
		]
	},
	// The text colour itself: black on the light ground, white on the dark one.
	{ value: 'neutral', label: 'Neutral', tone: TOKENS.primary }
] as const satisfies readonly { value: string; label: string; tone: readonly [Lch, Lch] }[];
export type Accent = (typeof ACCENTS)[number]['value'];
export const DEFAULT_ACCENT: Accent = 'emerald';

/** The variables an accent sets: the accent itself and the focus rings. All are in `MAPPED_VARS`. */
export const ACCENT_VARS = ['brand', 'ring', 'sidebar-ring'];

/** A token as CSS writes it. */
export const css = (v: Lch | readonly number[]): string =>
	`oklch(${v[0]} ${v[1]} ${v[2]}${v.length === 4 ? ` / ${v[3]}` : ''})`;

const toneOf = (accent: Accent, light: boolean): Lch =>
	(ACCENTS.find((a) => a.value === accent) ?? ACCENTS[0]).tone[light ? 0 : 1] as Lch;

/** The accent as a CSS colour, in the light or the dark theme. */
export const accentColor = (accent: Accent, light: boolean): string => css(toneOf(accent, light));

/** What to set on the root for an accent; nothing for the default, which the stylesheet holds. */
export function accentVars(accent: Accent, light: boolean): Record<string, string> {
	if (accent === DEFAULT_ACCENT) return {};
	const value = accentColor(accent, light);
	return Object.fromEntries(ACCENT_VARS.map((v) => [`--${v}`, value]));
}

/** The built-in theme with an accent, as opaque colours, for the contrast check. */
export function withAccent(accent: Accent, light: boolean): Palette {
	const [l, c, h] = toneOf(accent, light);
	const p = builtin(light);
	p.brand = p.ring = oklch(l, c, h);
	return p;
}

// --- what has to be readable -------------------------------------------------------------------

export const TEXT_FLOOR = 4.5;
export const CONTROL_FLOOR = 3;

/**
 * The surfaces running text and glyphs sit on. A hovered or chosen row is one of them too: in this
 * system a wash is an opaque step (`muted`, `accent`, `sidebar-accent`), not an alpha layer.
 */
export const SURFACES = [
	'background',
	'card',
	'popover',
	'sidebar',
	'muted',
	'secondary',
	'accent',
	'sidebar-accent'
];
/** Where red is written as words: the page, a card, a dialog. Not on a wash (see the components). */
const PLAIN = ['background', 'card', 'popover'];

const on = (fgs: string[], bgs: string[], floor: number) =>
	fgs.flatMap((t) => bgs.map((s): [string, string, number] => [t, s, floor]));

/** Every pair the page relies on: `[what is drawn, what it is drawn on, the floor]`. */
export const PAIRS: [string, string, number][] = [
	...on(['foreground', 'muted-foreground', 'success', 'warning'], SURFACES, TEXT_FLOOR),
	...on(['destructive'], PLAIN, TEXT_FLOOR),
	['secondary-foreground', 'secondary', TEXT_FLOOR],
	['primary-foreground', 'primary', TEXT_FLOOR],
	// code is drawn on the page and on a card
	...on(
		['syn-text', 'syn-key', 'syn-string', 'syn-number', 'syn-keyword', 'syn-fn'],
		['background', 'card'],
		TEXT_FLOOR
	),
	// controls: the focus ring, a filled button against the page, a status mark, a meter on its track
	...on(['ring', 'primary'], ['background', 'card', 'popover', 'sidebar'], CONTROL_FLOOR),
	...on(
		['success', 'warning', 'destructive'],
		['background', 'card', 'popover', 'sidebar', 'sidebar-accent'],
		CONTROL_FLOOR
	),
	...on(['brand'], ['background', 'card', 'muted'], CONTROL_FLOOR)
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

/** How far each surface sits from the page in lightness (OKLab), [light, dark]: sin-api's ladder. */
const LADDER: Record<string, [number, number]> = {
	card: [0.015, 0.025],
	popover: [0.015, 0.025],
	sidebar: [-0.015, 0.015],
	muted: [-0.025, 0.065],
	secondary: [-0.025, 0.065],
	accent: [-0.045, 0.095],
	'sidebar-accent': [-0.055, 0.075]
};

/**
 * Terminal role -> the variables it sets. Roles the page has no token for (code_bg, the diff
 * colours, thinking, bash, the mode colours) are not mapped: a diff line is a tint of
 * `success` or `destructive`, a code block sits on the page.
 */
const ROLE_VARS: Record<string, string[]> = {
	text: ['foreground', 'secondary-foreground', 'primary'],
	muted: ['muted-foreground'],
	accent: ['brand', 'ring'],
	success: ['success'],
	warning: ['warning'],
	error: ['destructive'],
	// the person's own message
	user_bg: ['secondary'],
	user_text: ['secondary-foreground'],
	// a hovered or chosen row
	selected_bg: ['accent', 'sidebar-accent']
};

/** Variables that hold the same colour as another: sin-api writes each out, so each is set. */
const FOLLOWERS: Record<string, string[]> = {
	foreground: [
		'card-foreground',
		'popover-foreground',
		'accent-foreground',
		'sidebar-foreground',
		'sidebar-accent-foreground'
	],
	primary: ['sidebar-primary'],
	'primary-foreground': ['sidebar-primary-foreground'],
	ring: ['sidebar-ring']
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
	if (wanted.foreground && wanted.background) wanted['primary-foreground'] = wanted.background;

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
	for (const name of Object.keys(wanted)) {
		if (fellBack.has(name)) continue;
		for (const v of [name, ...(FOLLOWERS[name] ?? [])]) vars[`--${v}`] = toHex(p[name]);
	}

	// Hairlines have no floor of their own: they follow the terminal when it names them.
	const hairline = parseHex(theme.roles.border_muted);
	if (hairline) {
		vars['--border'] = `${toHex(hairline)}73`;
		vars['--sidebar-border'] = `${toHex(hairline)}73`;
		vars['--input'] = `${toHex(hairline)}99`;
	}

	return { vars, fellBack: [...fellBack].sort(), light, palette: p };
}

/** Every variable `mapTheme` may set, so that leaving "Match terminal" can clear them. */
export const MAPPED_VARS: string[] = [
	...new Set([
		'background',
		'primary-foreground',
		...Object.keys(LADDER),
		...Object.values(ROLE_VARS).flat(),
		...Object.values(FOLLOWERS).flat(),
		'border',
		'sidebar-border',
		'input'
	])
].map((n) => `--${n}`);

// --- a small picture of a theme ------------------------------------------------------------------

/** The colours the theme cards of Settings > General draw their miniature page with. */
export const PREVIEW_TOKENS = [
	'background',
	'sidebar',
	'sidebar-accent',
	'card',
	'secondary',
	'foreground',
	'muted-foreground',
	'border',
	'brand'
] as const;
export type Preview = Record<(typeof PREVIEW_TOKENS)[number], string>;

/**
 * A theme as CSS colours: the built-in light or dark one with the accent in use, or what
 * "Match terminal" made of the terminal's theme (which brings its own accent).
 */
export function preview(light: boolean, accent: Accent, mapped?: Mapped | null): Preview {
	const out = {} as Preview;
	const side = mapped ? mapped.light : light;
	for (const name of PREVIEW_TOKENS) out[name] = css(TOKENS[name][side ? 0 : 1]);
	if (!mapped) return { ...out, brand: accentColor(accent, light) };
	for (const name of PREVIEW_TOKENS) {
		const set = mapped.vars[`--${name}`];
		if (set) out[name] = set;
	}
	return out;
}
