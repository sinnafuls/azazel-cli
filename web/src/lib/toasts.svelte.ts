// Short notices, shown by Sonner (`<Toaster>` in the root layout). Errors stay twice as long.
import { toast } from 'svelte-sonner';

export interface ToastItem {
	id: number;
	level: 'info' | 'warn' | 'error' | 'success';
	text: string;
}
class Toasts {
	/** What was shown last, newest last: kept for tests and for anything that reads them back. */
	items = $state<ToastItem[]>([]);
	private n = 0;
	push(text: string, level: ToastItem['level'] = 'info', ms = 5000) {
		const id = ++this.n;
		this.items = [...this.items.slice(-3), { id, level, text }];
		const duration = level === 'error' ? ms * 2 : ms;
		const opts = {
			id,
			duration,
			onDismiss: () => this.forget(id),
			onAutoClose: () => this.forget(id)
		};
		if (level === 'error') toast.error(text, opts);
		else if (level === 'warn') toast.warning(text, opts);
		else if (level === 'success') toast.success(text, opts);
		else toast.info(text, opts);
	}
	/** A notice that stays until `dismiss(id)`, for something that takes a while; `cancel` adds a button. */
	loading(text: string, cancel?: { label: string; onClick: () => void }): number {
		const id = ++this.n;
		toast.loading(text, {
			id,
			duration: Infinity,
			cancel: cancel && { label: cancel.label, onClick: cancel.onClick }
		});
		return id;
	}
	dismiss(id: number) {
		toast.dismiss(id);
		this.forget(id);
	}
	private forget(id: number) {
		this.items = this.items.filter((t) => t.id !== id);
	}
}
export const toasts = new Toasts();
