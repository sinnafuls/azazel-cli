export interface ToastItem {
	id: number;
	level: 'info' | 'warn' | 'error' | 'success';
	text: string;
}
class Toasts {
	items = $state<ToastItem[]>([]);
	private n = 0;
	push(text: string, level: ToastItem['level'] = 'info', ms = 5000) {
		const id = ++this.n;
		this.items = [...this.items.slice(-3), { id, level, text }];
		setTimeout(() => this.dismiss(id), level === 'error' ? ms * 2 : ms);
	}
	dismiss(id: number) {
		this.items = this.items.filter((t) => t.id !== id);
	}
}
export const toasts = new Toasts();
