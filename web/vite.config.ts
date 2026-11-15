import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vitest/config';

// The azazel server stays strict about Origin and Host, so the dev proxy rewrites them.
const target = process.env.AZAZEL_REMOTE ?? 'http://127.0.0.1:7420';

export default defineConfig({
	plugins: [tailwindcss(), sveltekit()],
	server: {
		proxy: {
			'/api': {
				target,
				ws: true,
				changeOrigin: true,
				configure(proxy) {
					proxy.on('proxyReqWs', (req) => req.setHeader('Origin', target));
					proxy.on('proxyReq', (req) => req.setHeader('Origin', target));
				}
			}
		}
	},
	test: {
		include: ['tests/unit/**/*.test.ts'],
		environment: 'jsdom',
		alias: { $lib: new URL('./src/lib', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1') }
	},
	resolve: process.env.VITEST ? { conditions: ['browser'] } : undefined
});
