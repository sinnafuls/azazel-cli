// A toast sits above a dialog but outside its content, so a click on it counts as "outside" and
// would close the dialog beneath it. Give this to `onInteractOutside` of every layer that dismisses.
export function outsideToasts<E extends Event>(next?: (e: E) => void) {
	return (e: E) => {
		if (e.target instanceof Element && e.target.closest('[data-sonner-toaster]')) {
			e.preventDefault();
			return;
		}
		next?.(e);
	};
}
