import { mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname } from 'node:path';
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig, type Plugin } from 'vite';

// Deployed behind a path prefix (e.g. BASE_PATH=/coversync); empty for dev and Tauri.
const base = (process.env.BASE_PATH ?? '') as '' | `/${string}`;

/**
 * Lists the npm packages whose code or files end up in the client bundle, in
 * .svelte-kit/bundled-packages.json, for packaging/notices/generate.sh.
 */
function bundledPackages(): Plugin {
	const vite = createRequire(import.meta.url).resolve('vite/package.json');
	// Helpers the build tools add as virtual modules.
	const virtual: [string, string][] = [
		['\0vite/', dirname(vite)],
		['\0rolldown/', dirname(createRequire(vite).resolve('rolldown/package.json'))]
	];
	return {
		name: 'coversync:bundled-packages',
		applyToEnvironment: (environment) => environment.name === 'client',
		generateBundle(_, bundle) {
			const dirs = new Set<string>();
			for (const file of Object.values(bundle)) {
				for (const id of file.type === 'chunk' ? file.moduleIds : file.originalFileNames) {
					// The innermost node_modules/<package>: pnpm nests them, and links some.
					const dir =
						/^((?:.*\/)?node_modules\/(?:@[^/]+\/)?[^/]+)\//.exec(id)?.[1] ??
						virtual.find(([prefix]) => id.startsWith(prefix))?.[1];
					if (dir) dirs.add(realpathSync(dir));
				}
			}
			const packages = [...dirs].map((dir) => {
				const { name, version, license } = JSON.parse(readFileSync(`${dir}/package.json`, 'utf8'));
				return { name, version, license, dir };
			});
			packages.sort((a, b) => a.name.localeCompare(b.name));
			mkdirSync('.svelte-kit', { recursive: true });
			writeFileSync('.svelte-kit/bundled-packages.json', JSON.stringify(packages, null, '\t'));
		}
	};
}

export default defineConfig({
	server: {
		proxy: { '/api': 'http://127.0.0.1:8770' }
	},
	plugins: [
		bundledPackages(),
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			adapter: adapter(),
			paths: { base }
		})
	]
});
