// The desktop app around the page, when there is one.
//
// The app's window loads the UI through a proxy of its own, which says so in
// a `Coversync-App` header on /api/hello and answers the app's commands
// itself, under /__coversync-app/v1/ (crates/coversync-proxy/src/app.rs). In
// a browser there is no such header, and the path is never asked for: there
// it would reach the server.
//
// What the app offers is a list of features, asked for once
// (crates/coversync-desktop-core/src/bridge.rs). Whatever depends on one is
// used only when it's listed: an older app lists fewer, and a newer one may
// list some this UI doesn't know.

import { hello } from './api';

/** The features this UI knows how to use. */
export type AppFeature = 'notify';

/** The version of the app's interface this UI speaks. */
const VERSION = '1';
const COMMANDS = `/__coversync-app/v${VERSION}/`;

/** Run one of the app's commands. Only once `hello` has said the app is there. */
async function command<T>(name: string, args: Record<string, unknown> = {}): Promise<T> {
	const res = await fetch(COMMANDS + name, {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify(args),
		cache: 'no-store'
	});
	if (!res.ok) throw new Error(`${name}: ${res.status}`);
	return res.json();
}

class DesktopApp {
	#features = $state<ReadonlySet<string>>(new Set());
	#asking: Promise<void> | undefined;

	/** Whether the page is in an app that offers `feature`. False in a browser, and until the app has answered. */
	has(feature: AppFeature) {
		return this.#features.has(feature);
	}

	/**
	 * Find the app and ask what it offers. It's asked once: later calls wait
	 * for the same answer, or ask again if that one couldn't be had (the
	 * server was down).
	 */
	detect(): Promise<void> {
		this.#asking ??= this.#ask().catch(() => {
			this.#asking = undefined;
		});
		return this.#asking;
	}

	async #ask() {
		const { app } = await hello();
		const speaks = app?.split(',').some((version) => version.trim() === VERSION);
		if (!speaks) return;
		const answer = await command<{ features?: unknown }>('capabilities');
		const features = Array.isArray(answer.features) ? answer.features : [];
		this.#features = new Set(features.filter((feature) => typeof feature === 'string'));
	}
}

export const desktopApp = new DesktopApp();
