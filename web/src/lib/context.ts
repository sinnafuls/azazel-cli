import { getContext, setContext } from 'svelte';
import type { Session } from '$lib/session/session.svelte';

const KEY = Symbol('session');
export const setSession = (s: Session) => setContext(KEY, s);
export const getSession = () => getContext<Session>(KEY);
