import { describe, expect, it } from 'vitest';
import { modelLabel, pickableRoles, shortModel, splitModel } from '$lib/models';
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

import { describeSteps, stepVerb } from '$lib/tools';

describe('a run of tool calls', () => {
	it('is told in a few words, kind by kind in the order they came', () => {
		expect(describeSteps(['write', 'shell', 'shell'])).toBe('Created a file, ran 2 commands');
		expect(describeSteps(['shell'])).toBe('Ran a command');
		expect(describeSteps(['read', 'grep', 'read', 'mystery'])).toBe(
			'Read 2 files, searched the code, used a tool'
		);
		expect(stepVerb('edit', true)).toBe('Editing');
		expect(stepVerb('shell', false)).toBe('Ran');
	});
});

describe('the short model name on the chip', () => {
	it('drops the vendor prefix, the organisation path and the version suffix', () => {
		expect(shortModel('anthropic.claude-sonnet-4-20250514-v1:0')).toBe('claude-sonnet-4');
		expect(shortModel('us.anthropic.claude-opus-4-v1:0')).toBe('claude-opus-4');
		expect(shortModel('meta-llama/llama-3.1-70b')).toBe('llama-3.1-70b');
		expect(shortModel('gpt-5')).toBe('gpt-5');
	});
});
