import { describe, expect, it } from 'vitest';
import { modelLabel, pickableRoles, splitModel } from '$lib/models';
import type { Roles } from '$lib/protocol';

describe('the model chip', () => {
	it('names the role the lead follows and the model it answers with', () => {
		const st = {
			chain: ['@lead'],
			model: 'deepseek/deepseek-chat',
			roles: { lead: 'deepseek/deepseek-reasoner', smol: 'deepseek/deepseek-chat' }
		};
		expect(modelLabel(st)).toEqual({ role: 'lead', model: 'deepseek/deepseek-reasoner' });
		// A role the state does not resolve falls back to the model that answered last.
		expect(modelLabel({ ...st, roles: {} })).toEqual({
			role: 'lead',
			model: 'deepseek/deepseek-chat'
		});
	});

	it('shows a model chosen by name as it is', () => {
		expect(modelLabel({ chain: ['openai/gpt-6-sol'], model: null })).toEqual({
			role: null,
			model: 'openai/gpt-6-sol'
		});
		expect(modelLabel(null)).toEqual({ role: null, model: '…' });
		expect(splitModel('openai/gpt-6-sol')).toEqual({ provider: 'openai/', id: 'gpt-6-sol' });
		expect(splitModel('…')).toEqual({ provider: '', id: '…' });
	});
});

describe('the roles of the picker', () => {
	const roles: Roles = {
		project: false,
		presets: [],
		roles: [
			{
				role: 'lead',
				chain: ['@default'],
				source: 'builtin',
				resolved: ['deepseek/deepseek-chat']
			},
			{ role: 'smol', chain: [], source: 'home', resolved: ['deepseek/deepseek-lite'] },
			{ role: 'vision', chain: ['openai/gpt-6'], source: 'builtin', resolved: [] }
		]
	};

	it('offers the roles that resolve to a usable model, searched by name or model', () => {
		expect(pickableRoles(roles, '').map((r) => r.role)).toEqual(['lead', 'smol']);
		expect(pickableRoles(roles, 'LITE').map((r) => r.role)).toEqual(['smol']);
		expect(pickableRoles(roles, '@lead').map((r) => r.role)).toEqual(['lead']);
		expect(pickableRoles(null, '')).toEqual([]);
	});
});
