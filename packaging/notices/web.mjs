// Prints the licenses of the npm packages in coversync's web UI, from the
// list web/vite.config.ts writes when the UI is built.
//
//   node packaging/notices/web.mjs web/.svelte-kit/bundled-packages.json

import { readdirSync, readFileSync } from 'node:fs';

const rule = '-'.repeat(80);
const packages = JSON.parse(readFileSync(process.argv[2], 'utf8'));
for (const { name, version, license, dir } of packages) {
	const files = readdirSync(dir).filter((f) => /^(licen[cs]e|copying)(\.\w+)?$/i.test(f));
	if (!files.length) throw new Error(`${name} has no license file in ${dir}`);
	// Licenses of code the package took in, such as rolldown's runtime helpers.
	files.push(...readdirSync(dir).filter((f) => /^third-party-licen[cs]e(\.\w+)?$/i.test(f)));
	const texts = files.map((f) =>
		// Vite's LICENSE.md goes on to the dependencies Vite itself bundles.
		readFileSync(`${dir}/${f}`, 'utf8')
			.split(/^# Licenses of bundled dependencies/m)[0]
			.trim()
	);
	console.log(`${rule}\n${name} ${version} (${license})\n\n${texts.join('\n\n')}\n`);
}
