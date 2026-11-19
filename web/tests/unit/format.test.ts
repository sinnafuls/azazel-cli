import { describe, expect, it } from 'vitest';
import { cacheLeft } from '$lib/format';

describe('the cache clock', () => {
	it('counts whole minutes and seconds down and ends cold', () => {
		expect(cacheLeft(252)).toBe('4:12');
		expect(cacheLeft(251.9)).toBe('4:11');
		expect(cacheLeft(5)).toBe('0:05');
		expect(cacheLeft(3600)).toBe('60:00');
		expect(cacheLeft(0)).toBe('cold');
		expect(cacheLeft(-3)).toBe('cold');
	});
});
