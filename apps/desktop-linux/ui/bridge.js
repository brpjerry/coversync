// How the fallback page reaches the app: through events. A script of the app's own, in a script
// world of its own that a server's pages never get, passes them on to the app and its answers
// back (../src/relay.js).

/** How long an answer can take: a check gives up after 15 seconds, so this is plenty. */
const TIMEOUT_MS = 60_000;
/**
 * Commands that may wait for the user: the keychain can ask for its password, for as long as
 * that takes. They have no deadline: the app goes on with them whatever the page thinks, so
 * giving up here would only show a failure for something that then happens.
 */
const MAY_ASK = new Set(['connect', 'pair', 'forget']);

let next = 0;
const waiting = new Map();

document.addEventListener('coversync-response', (event) => {
	const { id, ok, value, error } = JSON.parse(event.detail);
	const call = waiting.get(id);
	if (!call) return;
	waiting.delete(id);
	clearTimeout(call.timer);
	if (ok) call.resolve(value);
	else call.reject(error);
});

/** Runs the app's command `cmd`; rejects with the app's `Problem` (crates/coversync-desktop-core). */
export async function invoke(cmd, args = {}) {
	return new Promise((resolve, reject) => {
		const id = ++next;
		const timer = MAY_ASK.has(cmd)
			? undefined
			: setTimeout(() => {
					waiting.delete(id);
					reject({ title: 'Something went wrong', message: "The app didn't answer." });
				}, TIMEOUT_MS);
		waiting.set(id, { resolve, reject, timer });
		document.dispatchEvent(
			new CustomEvent('coversync-request', { detail: JSON.stringify({ id, cmd, args }) })
		);
	});
}
