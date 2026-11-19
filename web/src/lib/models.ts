// What the model picker shows: the lead's role or model, and the roles to choose from.
import type { RoleRow, Roles, State } from '$lib/protocol';

export interface ModelLabel {
	/** The role the lead follows (`lead`, `smol`), when its chain starts with one. */
	role: string | null;
	/** The model that answers, as `provider/id`, or `…` while it is not known. */
	model: string;
}

/** The lead as the chip names it: `@role` with the model it resolves to, or the model alone. */
export function modelLabel(
	st: Pick<State, 'chain' | 'model' | 'roles'> | null | undefined
): ModelLabel {
	const first = st?.chain?.[0];
	if (first?.startsWith('@')) {
		const role = first.slice(1);
		return { role, model: st?.roles?.[role] ?? st?.model ?? '…' };
	}
	return { role: null, model: first ?? st?.model ?? '…' };
}

/** `provider/id` split for display: the provider (with its slash) is dimmed. */
export function splitModel(model: string): { provider: string; id: string } {
	const slash = model.indexOf('/');
	return slash > 0
		? { provider: model.slice(0, slash + 1), id: model.slice(slash + 1) }
		: { provider: '', id: model };
}

/**
 * A model id without what only routes it: an organisation path (`meta-llama/`), a vendor prefix
 * (`anthropic.`, `us.anthropic.`), a version or date suffix (`-v1:0`, `-20250514`).
 */
export function shortModel(id: string): string {
	const last = id.slice(id.lastIndexOf('/') + 1);
	const noVendor = last.replace(/^(?:[a-z]{2}\.)?[a-z0-9-]+\.(?=[a-z])/i, '');
	return noVendor.replace(/-v\d+(?::\d+)?$/, '').replace(/-\d{8}$/, '') || id;
}

/** The roles that resolve to a usable model, matching `query` by name or by that model. */
export function pickableRoles(roles: Roles | null | undefined, query: string): RoleRow[] {
	const q = query.trim().toLowerCase();
	return (roles?.roles ?? []).filter(
		(r) => r.resolved.length > 0 && `@${r.role} ${r.resolved[0]}`.toLowerCase().includes(q)
	);
}
