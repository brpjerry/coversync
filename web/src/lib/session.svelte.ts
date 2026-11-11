// Signing in. The API is the boundary: any call it answers with 401 lands
// here, and the layout shows the login view. A session that ends while the
// app is open keeps the app, and its unsaved edits, under the login view.

import { api, ApiError, BASE, onUnauthorized, type Access } from './api';

/** Why the login view is up: the API's `code` for its 401. */
export type SignedOut = 'no_password' | 'login_required' | 'session_expired';

/** The session cookie is limited to the UI's own path, never the whole host. */
const cookiePath = () => `${BASE}/`;

/** "in 40 seconds", "in 3 minutes". */
function inAbout(seconds: number) {
	if (!(seconds > 0)) return 'shortly';
	if (seconds < 90) return `in ${Math.ceil(seconds)} seconds`;
	return `in ${Math.ceil(seconds / 60)} minutes`;
}

class Session {
	/** `checking` until the API first answers. */
	state = $state<'checking' | 'in' | 'out'>('checking');
	why = $state<SignedOut>('login_required');
	/** The app has been shown since the page loaded; it stays under the login view. */
	shown = $state(false);
	/** Bumped by each sign-in once the app has been shown, so it can reload what failed meanwhile. */
	generation = $state(0);
	/**
	 * How the API was last seen to take this page's requests. `token` in a desktop app paired
	 * with the server: its window is signed in as that device, with nothing to sign out of here.
	 */
	via = $state<Access['via'] | null>(null);

	constructor() {
		onUnauthorized((code) => this.lost(code));
	}

	/** Ask the API whether this browser is signed in. */
	async check() {
		try {
			this.via = (await api.session()).via;
			this.#signedIn();
		} catch (e) {
			// A 401 has called lost(). Otherwise coversyncd can't be reached:
			// the app says so itself.
			if (!(e instanceof ApiError && e.status === 401) && this.state === 'checking')
				this.#signedIn();
		}
	}

	lost(code: string) {
		this.why = code === 'no_password' || code === 'session_expired' ? code : 'login_required';
		this.state = 'out';
	}

	#signedIn() {
		if (this.state === 'out' && this.shown) this.generation++;
		this.state = 'in';
		this.shown = true;
	}

	/** Sign in; returns what to tell the user if that didn't work. */
	async login(password: string): Promise<string | null> {
		let res: Response;
		try {
			res = await fetch(`${BASE}/api/login`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ password, path: cookiePath() })
			});
		} catch {
			return 'Could not reach coversyncd.';
		}
		if (res.ok) {
			this.#signedIn();
			return null;
		}
		const body = await res.json().catch(() => ({}));
		if (body.code === 'no_password') {
			this.lost(body.code);
			return null;
		}
		if (res.status === 429)
			return `Too many failed attempts. Try again ${inAbout(body.retry_after)}.`;
		return body.error ?? `Signing in failed (${res.status}).`;
	}

	/** End this browser's session, then start afresh: nothing of the library stays on screen. */
	async logout(): Promise<boolean> {
		try {
			const res = await fetch(`${BASE}/api/logout`, {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ path: cookiePath() })
			});
			if (!res.ok) return false;
		} catch {
			return false;
		}
		location.reload();
		return true;
	}
}

export const session = new Session();
