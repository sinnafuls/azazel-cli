// SvelteKit's static adapter writes one inline <script> into every HTML file to start the app. The
// azazel server sends `script-src 'self'` (docs/remote.md), which forbids inline scripts, so this
// moves each one into a content-hashed file next to the other assets and points a <script src> at it.
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, statSync, writeFileSync, mkdirSync } from 'node:fs';
import { join, resolve } from 'node:path';

const build = resolve(import.meta.dir, '..', 'build');
const out = join(build, '_app', 'immutable', 'boot');
mkdirSync(out, { recursive: true });

function* html(dir: string): Generator<string> {
	for (const name of readdirSync(dir)) {
		const p = join(dir, name);
		if (statSync(p).isDirectory()) yield* html(p);
		else if (name.endsWith('.html')) yield p;
	}
}

let moved = 0;
for (const file of html(build)) {
	const src = readFileSync(file, 'utf8');
	const next = src.replace(/<script>([\s\S]*?)<\/script>/g, (_all, body: string) => {
		const hash = createHash('sha256').update(body).digest('hex').slice(0, 12);
		writeFileSync(join(out, `${hash}.js`), body.trim() + '\n');
		moved++;
		return `<script src="/_app/immutable/boot/${hash}.js"></script>`;
	});
	if (next !== src) writeFileSync(file, next);
}
const left = [...html(build)].filter((f) =>
	/<script(?![^>]*\bsrc=)[^>]*>/.test(readFileSync(f, 'utf8'))
);
if (left.length) {
	console.error(`inline scripts remain in: ${left.join(', ')}`);
	process.exit(1);
}
console.log(`moved ${moved} inline script(s) out of the HTML`);
