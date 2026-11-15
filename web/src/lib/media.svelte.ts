// Viewport facts the layout needs in script (the rest is CSS).
class Viewport {
	wide = $state(false); // >= 768: sheets dock beside the chat instead of covering the screen
	lg = $state(false); // >= 1024: the agents and usage pane has room
	coarse = $state(false); // a touch screen: Enter in the composer makes a new line
	init() {
		const watch = (q: string, set: (v: boolean) => void) => {
			const m = matchMedia(q);
			set(m.matches);
			m.addEventListener('change', (e) => set(e.matches));
		};
		watch('(min-width: 768px)', (v) => (this.wide = v));
		watch('(min-width: 1024px)', (v) => (this.lg = v));
		watch('(pointer: coarse)', (v) => (this.coarse = v));
	}
}
export const viewport = new Viewport();
