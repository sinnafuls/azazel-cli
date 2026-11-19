// Markdown from the model is untrusted. It is parsed with marked, highlighted, then sanitised with
// DOMPurify against a short allowlist before it is ever put into the page.
import DOMPurify from 'dompurify';
import hljs from 'highlight.js/lib/core';
import { Marked, type Tokens } from 'marked';

type Loader = () => Promise<{ default: Parameters<typeof hljs.registerLanguage>[1] }>;
const languages: Record<string, { names: string[]; load: Loader }> = {
	javascript: {
		names: ['js', 'jsx', 'mjs'],
		load: () => import('highlight.js/lib/languages/javascript')
	},
	typescript: { names: ['ts', 'tsx'], load: () => import('highlight.js/lib/languages/typescript') },
	json: { names: ['jsonc'], load: () => import('highlight.js/lib/languages/json') },
	bash: {
		names: ['sh', 'shell', 'zsh', 'console'],
		load: () => import('highlight.js/lib/languages/bash')
	},
	rust: { names: ['rs'], load: () => import('highlight.js/lib/languages/rust') },
	python: { names: ['py'], load: () => import('highlight.js/lib/languages/python') },
	go: { names: ['golang'], load: () => import('highlight.js/lib/languages/go') },
	diff: { names: ['patch'], load: () => import('highlight.js/lib/languages/diff') },
	yaml: { names: ['yml'], load: () => import('highlight.js/lib/languages/yaml') },
	css: { names: [], load: () => import('highlight.js/lib/languages/css') },
	xml: {
		names: ['html', 'svg', 'svelte', 'vue'],
		load: () => import('highlight.js/lib/languages/xml')
	},
	ini: { names: ['toml'], load: () => import('highlight.js/lib/languages/ini') },
	sql: { names: [], load: () => import('highlight.js/lib/languages/sql') },
	markdown: { names: ['md'], load: () => import('highlight.js/lib/languages/markdown') }
};
const alias = new Map<string, string>();
for (const [name, { names }] of Object.entries(languages)) {
	alias.set(name, name);
	for (const n of names) alias.set(n, name);
}
const loaded = new Set<string>();
const cache = new Map<string, string>();

/** Loads the highlighters the fenced blocks of `source` ask for. Resolves true when something new loaded. */
export async function loadLanguages(source: string): Promise<boolean> {
	const wanted = new Set<string>();
	for (const m of source.matchAll(/^[ \t]*(?:```|~~~)\s*([\w+-]+)/gm)) {
		const name = alias.get(m[1].toLowerCase());
		if (name && !loaded.has(name)) wanted.add(name);
	}
	await Promise.all(
		[...wanted].map(async (name) => {
			const mod = await languages[name].load();
			hljs.registerLanguage(name, mod.default);
			loaded.add(name);
		})
	);
	if (wanted.size) cache.clear();
	return wanted.size > 0;
}

/** Loads the highlighter of one language (`bash`, `ts`). Resolves true when it was not there before. */
export async function loadLanguage(lang: string): Promise<boolean> {
	const name = alias.get(lang.toLowerCase());
	if (!name || loaded.has(name)) return false;
	const mod = await languages[name].load();
	if (loaded.has(name)) return true;
	hljs.registerLanguage(name, mod.default);
	loaded.add(name);
	cache.clear();
	return true;
}

/** Code as safe HTML, coloured when its language is loaded (`loadLanguage`) and plain until then. */
export function highlightCode(text: string, lang: string): string {
	const name = alias.get(lang.toLowerCase());
	return sanitize(
		name && loaded.has(name)
			? hljs.highlight(text, { language: name, ignoreIllegals: true }).value
			: escape(text)
	);
}

const escape = (s: string) =>
	s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

const marked = new Marked({
	gfm: true,
	breaks: false,
	renderer: {
		code({ text, lang }: Tokens.Code) {
			const name = alias.get((lang ?? '').trim().split(/\s+/)[0].toLowerCase());
			let body = escape(text);
			let cls = '';
			if (name && loaded.has(name)) {
				body = hljs.highlight(text, { language: name, ignoreIllegals: true }).value;
				cls = ` class="hljs language-${name}"`;
			}
			return `<pre><code${cls}>${body}</code></pre>`;
		},
		// Raw HTML typed by the model is shown as text, not parsed.
		html({ text }: Tokens.HTML | Tokens.Tag) {
			return escape(text);
		},
		image({ text }: Tokens.Image) {
			return escape(text);
		}
	}
});

const SAFE_CLASS = /^(hljs(-[a-z_]+)?|language-[a-z0-9+-]+)$/;
let hooked = false;
function purifier() {
	if (!hooked) {
		hooked = true;
		DOMPurify.addHook('afterSanitizeAttributes', (node) => {
			if (node.hasAttribute('class')) {
				// A class from the model must not reach the page's own utility classes.
				const kept = (node.getAttribute('class') ?? '')
					.split(/\s+/)
					.filter((c) => SAFE_CLASS.test(c));
				if (kept.length) node.setAttribute('class', kept.join(' '));
				else node.removeAttribute('class');
			}
			if (node.tagName === 'A') {
				node.setAttribute('target', '_blank');
				node.setAttribute('rel', 'noopener noreferrer nofollow');
			}
		});
	}
	return DOMPurify;
}

const ALLOWED_TAGS = [
	'p',
	'br',
	'hr',
	'strong',
	'em',
	'del',
	'code',
	'pre',
	'blockquote',
	'ul',
	'ol',
	'li',
	'a',
	'span',
	'h1',
	'h2',
	'h3',
	'h4',
	'h5',
	'h6',
	'table',
	'thead',
	'tbody',
	'tr',
	'th',
	'td',
	'input'
];

export function sanitize(html: string): string {
	return purifier().sanitize(html, {
		ALLOWED_TAGS,
		ALLOWED_ATTR: ['href', 'title', 'class', 'start', 'type', 'checked', 'disabled'],
		ALLOW_DATA_ATTR: false,
		ALLOWED_URI_REGEXP: /^(?:https?|mailto):/i
	});
}

/** Renders markdown to safe HTML. */
export function renderMarkdown(source: string): string {
	return sanitize(marked.parse(source, { async: false }) as string);
}

/**
 * Renders a growing text as one HTML string per top-level block. A block that the text has moved past
 * is rendered once and served from a cache, so a long reply costs one render of its last block per
 * delta, not one of the whole reply.
 */
export function renderBlocks(source: string, final = false): string[] {
	const tokens = marked.lexer(source);
	const out: string[] = [];
	tokens.forEach((t, i) => {
		const open = !final && i === tokens.length - 1;
		if (t.type === 'space') return;
		if (open) return void out.push(renderMarkdown(t.raw));
		const hit = cache.get(t.raw);
		if (hit !== undefined) return void out.push(hit);
		const html = renderMarkdown(t.raw);
		if (cache.size > 800) cache.clear();
		cache.set(t.raw, html);
		out.push(html);
	});
	return out;
}
