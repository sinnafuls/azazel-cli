// Generates src/lib/protocol/rpc.ts from the repository's docs/rpc.schema.json.
// Every definition is listed as a property of one root object so that each becomes a named export.
import { compile } from 'json-schema-to-typescript';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';

const root = resolve(import.meta.dir, '..');
const schema = JSON.parse(await readFile(resolve(root, '../docs/rpc.schema.json'), 'utf8'));

// json-schema-to-typescript reads a schema without a type (any JSON value) as an object, and drops
// the members a `$ref` sits beside (`{ $ref: ThemeColors, properties: { type } }`). Say both plainly.
const ANNOTATIONS = new Set(['description', 'default', 'title']);
function plain(node: unknown): unknown {
	if (Array.isArray(node)) return node.map(plain);
	if (!node || typeof node !== 'object') return node;
	const o = Object.fromEntries(
		Object.entries(node as Record<string, unknown>).map(([k, v]) => [
			k,
			k === 'default' || k === 'const' || k === 'enum' ? v : plain(v)
		])
	);
	if (Object.keys(o).every((k) => ANNOTATIONS.has(k))) return { ...o, tsType: 'unknown' };
	if (
		typeof o.$ref === 'string' &&
		Object.keys(o).some((k) => !ANNOTATIONS.has(k) && k !== '$ref')
	) {
		const { $ref, description, ...rest } = o;
		return { description, allOf: [{ $ref }, rest] };
	}
	return o;
}
for (const [name, def] of Object.entries(schema.$defs)) schema.$defs[name] = plain(def);
const properties = Object.fromEntries(
	Object.keys(schema.$defs).map((name) => [name, { $ref: `#/$defs/${name}` }])
);
const everything = {
	$schema: schema.$schema,
	title: 'Rpc',
	description: schema.description,
	type: 'object',
	properties,
	additionalProperties: false,
	$defs: schema.$defs
};
const out = resolve(root, 'src/lib/protocol/rpc.ts');
const ts = await compile(everything, 'Rpc', {
	bannerComment: '/* Generated from docs/rpc.schema.json by `bun run gen:types`. Do not edit. */',
	additionalProperties: false,
	style: { singleQuote: true, useTabs: true, printWidth: 100 }
});
await mkdir(dirname(out), { recursive: true });
await writeFile(out, ts);
console.log(`wrote ${out}`);
