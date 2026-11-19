// Profiles as the page shows them: a profile is a named set of role chains (`State.profiles`), and
// a session moves between them by hand (`set_profile`) or when its provider runs out of allowance
// (`profile_changed`). Pure, so it can be tested without a page.
import type { FrameOf, ProfileInfo } from '$lib/protocol';
import { resetsIn } from '$lib/format';
import { shortModel, splitModel } from '$lib/models';

/** The roles a profile is told apart by: the lead's model, the careful one and the quick one. */
export const PROFILE_ROLES = ['default', 'slow', 'smol'] as const;

const SOURCE: Record<string, string> = {
	builtin: 'built-in',
	user: 'your config',
	project: 'this project'
};
/** Where a profile comes from, as its badge says it. */
export const profileSource = (p: Pick<ProfileInfo, 'source'>) => SOURCE[p.source] ?? p.source;

/** The model of each role of `PROFILE_ROLES` the profile sets, as `provider/id[:level]`. */
export function profileModels(p: Pick<ProfileInfo, 'roles'>): { role: string; model: string }[] {
	return PROFILE_ROLES.flatMap((role) => {
		const model = p.roles?.[role];
		return model ? [{ role, model }] : [];
	});
}

/** Those models on one line, each without its provider: `claude-opus-5-5 · claude-haiku-5-5`. */
export function profileSummary(p: Pick<ProfileInfo, 'roles'>): string {
	return profileModels(p)
		.map((m) => shortModel(splitModel(m.model).id))
		.join(' · ');
}

/** The commands that make an unusable profile usable: one `azazel login` per provider it lacks. */
export function profileLogins(p: Pick<ProfileInfo, 'usable' | 'missing_logins'>): string[] {
	if (p.usable) return [];
	const missing = p.missing_logins ?? [];
	return (missing.length ? missing : ['<provider>']).map((provider) => `azazel login ${provider}`);
}

/** A time of day for a reset that is near (`14:30`), with the day when it is not today. */
export function resetClock(unix: number, now = Date.now()): string {
	const at = new Date(unix * 1000);
	const time = at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
	return at.toDateString() === new Date(now).toDateString()
		? time
		: `${at.toLocaleDateString([], { weekday: 'short' })} ${time}`;
}

export interface ProfileNotice {
	level: 'info' | 'warn';
	text: string;
	/** How long it stays: the notice of a limit is long, and is read once. */
	ms: number;
}

const CACHE_COST = 'The next reply starts a new prompt cache, so it costs more.';

/**
 * What a `profile_changed` frame is worth saying. A switch somebody asked for says nothing: the
 * chip shows it. `model` is what answered before the switch, for the name of its provider.
 */
export function profileNotice(
	f: Pick<FrameOf<'profile_changed'>, 'to' | 'reason' | 'until' | 'limit'>,
	model?: string | null,
	now = Date.now()
): ProfileNotice | null {
	if (f.reason === 'limit') {
		const provider = model ? splitModel(model).provider.replace(/\/$/, '') : '';
		const until = f.until
			? `, which resets at ${resetClock(f.until, now)} (in ${resetsIn(f.until, now)})`
			: '';
		return {
			level: 'warn',
			text: `${provider || 'The provider'} reached its ${f.limit || 'usage limit'}${until}. Switched to the profile ${f.to}. ${CACHE_COST}`,
			ms: 12_000
		};
	}
	if (f.reason === 'reset')
		return {
			level: 'info',
			text: `The usage limit has reset: switched back to the profile ${f.to}. ${CACHE_COST}`,
			ms: 8000
		};
	return null;
}
