// One command to a session that is not the one on screen (a row of the sidebar, the settings window
// on the home page): connect, attach, send, close.
import { wsUrl } from '$lib/api';
import type { Command } from '$lib/protocol';
import { RpcClient } from './client';

export function oneShot<T = unknown>(id: string, command: Command, timeoutMs = 15_000) {
	return new Promise<T>((resolve, reject) => {
		let done = false;
		const finish = (fn: () => void) => {
			if (done) return;
			done = true;
			clearTimeout(timer);
			client.close();
			fn();
		};
		const client: RpcClient = new RpcClient({
			url: wsUrl(id),
			since: () => null,
			onFrame: () => {},
			onAttached: () => {
				client.send<T>(command).then(
					(v) => finish(() => resolve(v)),
					(e) => finish(() => reject(e))
				);
			},
			onStatus: (s, detail) => {
				// Ending a session closes its socket: that is the answer to `stop`.
				if (s === 'ended' && command.type === 'stop') finish(() => resolve(null as T));
				else if (s === 'ended' || s === 'unauthorized' || s === 'missing')
					finish(() => reject(new Error(detail ?? 'That session is not available any more.')));
			}
		});
		const timer = setTimeout(
			() => finish(() => reject(new Error('The computer did not answer.'))),
			timeoutMs
		);
		client.connect();
	});
}
