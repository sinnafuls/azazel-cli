// What a `ui_response` value looks like for each kind of question (docs/rpc.md, "ui_request").
import type { Question } from '$lib/protocol';

export type Answer = string | string[] | boolean | null;

/** The state of one question's input in a sheet. */
export interface Draft {
	chosen: string[];
	text: string;
	confirm: boolean | null;
}

export function emptyDraft(q: Pick<Question, 'recommended' | 'expects' | 'options'>): Draft {
	const rec = q.recommended ?? null;
	return {
		chosen: rec && q.options?.includes(rec) ? [rec] : [],
		text: '',
		confirm: rec === 'yes' ? true : rec === 'no' ? false : null
	};
}

/** The `value` for one question, or null when it is left open. */
export function answerOf(q: Pick<Question, 'expects' | 'allow_freeform'>, d: Draft): Answer {
	switch (q.expects) {
		case 'confirm':
			return d.confirm;
		case 'text':
			return d.text.trim() ? d.text : null;
		case 'select':
		case 'multiselect': {
			const extra = q.allow_freeform && d.text.trim() ? [d.text.trim()] : [];
			const all = q.expects === 'select' && extra.length ? extra : [...d.chosen, ...extra];
			return all.length ? all : null;
		}
		default:
			return null;
	}
}

export function answered(q: Pick<Question, 'expects' | 'allow_freeform'>, d: Draft): boolean {
	return answerOf(q, d) !== null;
}
