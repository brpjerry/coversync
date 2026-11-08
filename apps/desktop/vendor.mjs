// Copies the parts of @tauri-apps/api that the fallback page imports into
// ui/vendor, which is committed: the page has no build step, and Tauri
// serves ui/ as it is. Run it after changing @tauri-apps/api's version:
//
//     pnpm vendor
//
// CI runs it too and fails if ui/vendor differs from what it writes.

import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const from = join(here, 'node_modules/@tauri-apps/api');
const to = join(here, 'ui/vendor/tauri-api');

const files = [
	'core.js',
	'external/tslib/tslib.es6.js',
	'LICENSE-APACHE-2.0',
	'LICENSE-MIT',
	'package.json'
];

rmSync(to, { recursive: true, force: true });
for (const file of files) {
	mkdirSync(dirname(join(to, file)), { recursive: true });
	cpSync(join(from, file), join(to, file));
}
