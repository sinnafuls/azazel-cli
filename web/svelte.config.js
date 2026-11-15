import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/** @type {import('@sveltejs/kit').Config} */
export default {
	preprocess: vitePreprocess(),
	kit: {
		adapter: adapter({ pages: 'build', assets: 'build', fallback: 'index.html', strict: false }),
		// The page is served from the server's root for every route, so asset URLs must be absolute.
		paths: { relative: false }
	}
};
