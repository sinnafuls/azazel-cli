import { describe, expect, it } from 'vitest';
import { renderBlocks, renderMarkdown } from '$lib/markdown';

const attack = [
	'<script>window.pwned = 1</script>',
	'<img src=x onerror="window.pwned=2">',
	'[click](javascript:window.pwned=3)',
	'<a href="javascript:alert(1)" onclick="x()">link</a>',
	'<iframe src="https://evil.example"></iframe>',
	'<svg onload="window.pwned=4"><circle/></svg>',
	'![x](https://evil.example/track.png)',
	'<div class="fixed inset-0 bg-black z-50">overlay</div>',
	'<style>body{display:none}</style>',
	'<form action="https://evil.example"><input name=pw></form>',
	'```html\n<script>window.pwned=5</script>\n```'
].join('\n\n');

describe('model markdown is untrusted', () => {
	it('removes scripts, event handlers, javascript: links, frames, forms and tracking images', () => {
		const html = renderMarkdown(attack);
		const doc = new DOMParser().parseFromString(html, 'text/html');
		expect(
			doc.querySelectorAll('script, iframe, svg, style, form, img, [onerror], [onload], [onclick]')
		).toHaveLength(0);
		for (const a of doc.querySelectorAll('a'))
			expect(a.getAttribute('href') ?? '').not.toMatch(/^javascript:/i);
		expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
	});

	it('does not let the model borrow the page’s own classes', () => {
		const doc = new DOMParser().parseFromString(
			renderMarkdown('<div class="fixed inset-0 z-50">x</div>'),
			'text/html'
		);
		expect(doc.querySelector('.fixed, .inset-0, .z-50')).toBeNull();
	});

	it('shows a code block’s HTML as text', () => {
		const html = renderMarkdown('```html\n<script>alert(1)</script>\n```');
		expect(html).toContain('&lt;script&gt;');
		expect(html).not.toContain('<script');
	});

	it('opens links safely and keeps normal markdown', () => {
		const html = renderMarkdown(
			'## Title\n\n- **bold** item\n\n[docs](https://example.com)\n\n| a | b |\n|---|---|\n| 1 | 2 |'
		);
		const doc = new DOMParser().parseFromString(html, 'text/html');
		const a = doc.querySelector('a')!;
		expect(a.getAttribute('rel')).toContain('noopener');
		expect(a.getAttribute('target')).toBe('_blank');
		expect(doc.querySelector('h2')?.textContent).toBe('Title');
		expect(doc.querySelector('strong')?.textContent).toBe('bold');
		expect(doc.querySelector('table')).not.toBeNull();
	});

	it('sanitises every block while a reply is still streaming', () => {
		const parts = renderBlocks(
			'First paragraph.\n\n<img src=x onerror=alert(1)>\n\nStill typing <scr',
			false
		);
		expect(parts.length).toBeGreaterThanOrEqual(2);
		const doc = new DOMParser().parseFromString(parts.join(''), 'text/html');
		expect(doc.querySelectorAll('img, script, [onerror]')).toHaveLength(0);
		// What the model typed is still readable, as text.
		expect(doc.body.textContent).toContain('<img src=x');
	});

	it('serves finished blocks from a cache when the text grows', () => {
		const a = renderBlocks('One.\n\nTwo.', false);
		const b = renderBlocks('One.\n\nTwo.\n\nThree', false);
		expect(b[0]).toBe(a[0]);
		expect(b).toHaveLength(3);
	});
});
