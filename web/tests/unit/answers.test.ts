import { describe, expect, it } from 'vitest';
import { answerOf, emptyDraft } from '$lib/answers';
import { pairSecret, wsUrl } from '$lib/api';
import { fit } from '$lib/images';

describe('ui_response values', () => {
	it('starts on the recommended option and sends what each kind expects', () => {
		const sel = {
			expects: 'select' as const,
			options: ['sqlite', 'postgres'],
			recommended: 'postgres',
			allow_freeform: false
		};
		expect(answerOf(sel, emptyDraft(sel))).toEqual(['postgres']);
		const conf = { expects: 'confirm' as const, options: [], recommended: 'yes' };
		expect(answerOf(conf, emptyDraft(conf))).toBe(true);
		const text = { expects: 'text' as const, options: [] };
		expect(answerOf(text, emptyDraft(text))).toBeNull();
		expect(answerOf(text, { ...emptyDraft(text), text: ' hi ' })).toBe(' hi ');
	});

	it('lets a typed answer replace a single choice or join a multiselect', () => {
		const one = { expects: 'select' as const, options: ['a', 'b'], allow_freeform: true };
		expect(answerOf(one, { chosen: ['a'], text: 'mine', confirm: null })).toEqual(['mine']);
		const many = { expects: 'multiselect' as const, options: ['a', 'b'], allow_freeform: true };
		expect(answerOf(many, { chosen: ['a', 'b'], text: 'c', confirm: null })).toEqual([
			'a',
			'b',
			'c'
		]);
	});
});

describe('pairing and urls', () => {
	it('reads only a well-formed secret from the fragment', () => {
		expect(pairSecret('#pair=abc_DEF-123')).toBe('abc_DEF-123');
		expect(pairSecret('#pair=')).toBeNull();
		expect(pairSecret('#pair=a b')).toBeNull();
		expect(pairSecret('#other')).toBeNull();
	});
	it('picks wss on https', () => {
		expect(wsUrl('a b', { protocol: 'https:', host: 'h.ts.net' })).toBe(
			'wss://h.ts.net/api/ws/a%20b'
		);
		expect(wsUrl('x', { protocol: 'http:', host: '127.0.0.1:7420' })).toBe(
			'ws://127.0.0.1:7420/api/ws/x'
		);
	});
	it('fits images inside 2048 without upscaling', () => {
		expect(fit(4096, 2048)).toEqual({ w: 2048, h: 1024 });
		expect(fit(800, 600)).toEqual({ w: 800, h: 600 });
	});
});
