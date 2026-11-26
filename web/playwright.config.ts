import { defineConfig, devices } from '@playwright/test';

// The tests start their own `azazel remote` in front of a scripted background session (see
// tests/e2e/stack.ts), so there is no web server to configure and no fixed port.
export default defineConfig({
	testDir: 'tests/e2e',
	fullyParallel: true,
	// A CI runner has four cores and Playwright takes half by default; these tests mostly wait on
	// the server and the browser, so every core runs one.
	workers: process.env.CI ? 4 : undefined,
	forbidOnly: !!process.env.CI,
	retries: process.env.CI ? 1 : 0,
	reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : 'list',
	timeout: 60_000,
	expect: { timeout: 15_000 },
	use: { trace: 'retain-on-failure' },
	projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }]
});
