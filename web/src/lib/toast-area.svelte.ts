// Where toasts go: at the top of the column the page's header spans, under it. There a toast covers
// the first lines of what is read, never the header, the banner above it or the composer's buttons.
// A header (and the banner) is marked with `{@attach aboveToasts}`; the lowest of them is the edge.
class ToastArea {
	/** From the top of the window to the lower edge of the lowest marked element, in CSS pixels. */
	top = $state(0);
	/** From the right of the window to the right edge of that element. */
	right = $state(0);
	private nodes = new Set<HTMLElement>();
	private observer: ResizeObserver | null = null;

	private measure = () => {
		let top = 0;
		let right = 0;
		for (const node of this.nodes) {
			const r = node.getBoundingClientRect();
			if (r.height > 0 && r.bottom > top) {
				top = r.bottom;
				right = document.documentElement.clientWidth - r.right;
			}
		}
		this.top = Math.max(0, Math.round(top));
		this.right = Math.max(0, Math.round(right));
	};

	add(node: HTMLElement): () => void {
		if (!this.observer) {
			// One element growing moves the ones below it without resizing them: all are read again.
			this.observer = new ResizeObserver(this.measure);
			addEventListener('resize', this.measure);
		}
		this.nodes.add(node);
		this.observer.observe(node);
		this.measure();
		return () => {
			this.nodes.delete(node);
			this.observer?.unobserve(node);
			this.measure();
		};
	}
}
export const toastArea = new ToastArea();

/** Marks an element toasts appear under, within its right edge. */
export const aboveToasts = (node: HTMLElement) => toastArea.add(node);
