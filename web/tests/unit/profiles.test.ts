import { describe, expect, it } from 'vitest';
import type { AnyFrame, ProfileInfo, State } from '$lib/protocol';
import {
	profileLogins,
	profileModels,
	profileNotice,
	profileSource,
	profileSummary,
	resetClock
} from '$lib/profiles';
import { emptyModel, reduce, withState } from '$lib/session/reduce';
import { filterSlash, needsArgs, parseSlash, slashEntries } from '$lib/slash';

const claude: ProfileInfo = {
	name: 'claude',
	source: 'builtin',
	usable: true,
	missing_logins: [],
	roles: {
		coder: 'anthropic/claude-sonnet-5-5',
		default: 'anthropic/claude-opus-5-5',
		slow: 'anthropic/claude-fable-5-2',
		smol: 'anthropic/claude-haiku-5-5'
	}
};
const gpt: ProfileInfo = {
	name: 'gpt',
	source: 'builtin',
	usable: false,
	missing_logins: ['openai-codex'],
	roles: { default: 'openai-codex/gpt-6.1-sol', smol: 'openai-codex/gpt-6-luna' }
};
const cheap: ProfileInfo = {
	name: 'cheap',
	source: 'user',
	usable: true,
	missing_logins: [],
	roles: { default: 'anthropic/claude-sonnet-5-5' }
};

describe('a profile as the page lists it', () => {
	it('names the default, slow and smol models, in that order, and only those it sets', () => {
		expect(profileModels(claude)).toEqual([
			{ role: 'default', model: 'anthropic/claude-opus-5-5' },
			{ role: 'slow', model: 'anthropic/claude-fable-5-2' },
			{ role: 'smol', model: 'anthropic/claude-haiku-5-5' }
		]);
		expect(profileModels(gpt).map((m) => m.role)).toEqual(['default', 'smol']);
		expect(profileModels({ roles: undefined })).toEqual([]);
	});

	it('puts them on one line without their providers', () => {
		expect(profileSummary(claude)).toBe('claude-opus-5-5 · claude-fable-5-2 · claude-haiku-5-5');
		expect(profileSummary(cheap)).toBe('claude-sonnet-5-5');
	});

	it('says where a profile comes from, and passes on a source it does not know', () => {
		expect(profileSource(claude)).toBe('built-in');
		expect(profileSource(cheap)).toBe('your config');
		expect(profileSource({ source: 'project' })).toBe('this project');
		expect(profileSource({ source: 'package' })).toBe('package');
	});

	it('names the login an unusable profile needs, and none for a usable one', () => {
		expect(profileLogins(claude)).toEqual([]);
		expect(profileLogins(gpt)).toEqual(['azazel login openai-codex']);
		expect(profileLogins({ usable: false, missing_logins: ['a', 'b'] })).toEqual([
			'azazel login a',
			'azazel login b'
		]);
		// An unusable profile that names no provider still says what kind of command helps.
		expect(profileLogins({ usable: false, missing_logins: [] })).toEqual([
			'azazel login <provider>'
		]);
	});
});

describe('what a switch of profile says', () => {
	// Noon, local time, so that the day of a reset does not depend on the zone of the machine.
	const now = new Date(2026, 10, 19, 12, 0, 0).getTime();
	const unix = (ms: number) => Math.floor(ms / 1000);

	it('says nothing when somebody chose it', () => {
		expect(profileNotice({ to: 'cheap', reason: 'user' }, 'anthropic/claude-opus-5-5')).toBeNull();
	});

	it('names the provider, its limit, when it resets and the cost of the new cache', () => {
		const until = unix(now + (2 * 60 + 14) * 60_000);
		const n = profileNotice(
			{ to: 'cheap', reason: 'limit', until, limit: '5-hour limit' },
			'anthropic/claude-opus-5-5',
			now
		)!;
		expect(n.level).toBe('warn');
		expect(n.text).toContain('anthropic reached its 5-hour limit');
		expect(n.text).toContain(`resets at ${resetClock(until, now)} (in 2 h)`);
		expect(n.text).toContain('Switched to the profile cheap.');
		expect(n.text).toContain('new prompt cache');
		// Long enough to read: longer than the notice of a switch back.
		expect(n.ms).toBeGreaterThan(profileNotice({ to: 'claude', reason: 'reset' })!.ms);
	});

	it('still makes sense without a provider, a limit or a time', () => {
		const n = profileNotice({ to: 'cheap', reason: 'limit' }, null, now)!;
		expect(n.text).toBe(
			'The provider reached its usage limit. Switched to the profile cheap. The next reply starts a new prompt cache, so it costs more.'
		);
	});

	it('says a reset switched back', () => {
		const n = profileNotice({ to: 'claude', reason: 'reset' }, 'anthropic/claude-sonnet-5-5')!;
		expect(n.level).toBe('info');
		expect(n.text).toContain('switched back to the profile claude');
		expect(n.text).toContain('new prompt cache');
	});

	it('gives the day of a reset that is not today', () => {
		const today = resetClock(unix(now + 3600_000), now);
		const tomorrow = resetClock(unix(now + 26 * 3600_000), now);
		expect(today).toMatch(/\d{1,2}.\d{2}/);
		expect(tomorrow.endsWith(resetClock(unix(now + 2 * 3600_000), now))).toBe(true);
		expect(tomorrow.length).toBeGreaterThan(today.length);
	});
});

describe('profile_changed in the reducer', () => {
	const state = {
		model: 'anthropic/claude-opus-5-5',
		profile: 'claude',
		profiles: [claude, gpt, cheap]
	} as unknown as State;
	const frame = (f: Record<string, unknown>) =>
		({ type: 'profile_changed', from: 'claude', ...f }) as unknown as AnyFrame;
	const started = () => withState(emptyModel(), state);

	it('moves the chip at once and is quiet for a switch somebody chose', () => {
		const m = reduce(started(), frame({ to: 'cheap', reason: 'user' }));
		expect(m.state?.profile).toBe('cheap');
		// The list of profiles and the rest of the state stay as they were.
		expect(m.state?.profiles).toHaveLength(3);
		expect(m.state?.model).toBe('anthropic/claude-opus-5-5');
		expect(m.toasts).toEqual([]);
		expect(m.items).toEqual([]);
	});

	it('raises one warning toast for a limit, naming the provider that answered before', () => {
		const until = Math.floor(Date.now() / 1000) + 3 * 3600;
		const m = reduce(
			started(),
			frame({ to: 'cheap', reason: 'limit', until, limit: 'weekly limit', at: 7 })
		);
		expect(m.state?.profile).toBe('cheap');
		expect(m.toasts).toHaveLength(1);
		expect(m.toasts[0]).toMatchObject({ level: 'warn', ms: 12_000 });
		expect(m.toasts[0].text).toContain('anthropic reached its weekly limit');
		expect(m.toasts[0].text).toContain('(in 3 h)');
		expect(m.toasts[0].text).toContain('profile cheap');
	});

	it('raises an info toast when the limit resets and the session goes back', () => {
		let m = reduce(started(), frame({ to: 'cheap', reason: 'limit', at: 7 }));
		m = reduce(m, frame({ from: 'cheap', to: 'claude', reason: 'reset', at: 9 }));
		expect(m.state?.profile).toBe('claude');
		expect(m.toasts.map((t) => t.level)).toEqual(['warn', 'info']);
		expect(new Set(m.toasts.map((t) => t.id)).size).toBe(2);
	});

	it('keeps the toast when the frame comes before the first state', () => {
		const m = reduce(emptyModel(), frame({ to: 'cheap', reason: 'limit' }));
		expect(m.state).toBeNull();
		expect(m.toasts).toHaveLength(1);
		expect(m.toasts[0].text).toContain('The provider reached its usage limit');
	});

	it('takes the profile of a later state over the one a frame set', () => {
		let m = reduce(started(), frame({ to: 'cheap', reason: 'user' }));
		m = withState(m, { ...state, profile: 'claude' });
		expect(m.state?.profile).toBe('claude');
	});
});

describe('/profile and /preset in the / menu', () => {
	const entries = slashEntries([
		// The computer lists both as well: neither is shown twice.
		{ name: 'profile', description: 'from the computer', source: 'builtin' },
		{ name: 'preset', description: 'from the computer', source: 'builtin' }
	]);

	it('lists each once, as a command of the page that needs no argument', () => {
		for (const name of ['profile', 'preset']) {
			const found = entries.filter((e) => e.name === name);
			expect(found, name).toHaveLength(1);
			expect(found[0].builtin).toBe(true);
			expect(needsArgs(found[0])).toBe(false);
		}
		expect(filterSlash(entries, 'pr').map((e) => e.name)).toEqual(['profile', 'preset']);
	});

	it('parses a name and a save', () => {
		expect(parseSlash('/profile', entries)).toMatchObject({
			entry: { name: 'profile' },
			args: ''
		});
		expect(parseSlash('/profile cheap', entries)).toMatchObject({
			entry: { name: 'profile' },
			args: 'cheap'
		});
		expect(parseSlash('/preset save mine', entries)).toMatchObject({
			entry: { name: 'preset' },
			args: 'save mine'
		});
	});
});
