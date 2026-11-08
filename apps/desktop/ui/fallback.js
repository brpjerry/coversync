// The fallback page: shown when no server is set, it can't be reached, or
// the page crashed. Once a server answers, the app moves the window on to
// its UI.

// @tauri-apps/api's own module, copied into ui/vendor (see vendor.mjs): the app
// doesn't put the API in a global, so pages served through its proxies don't get it.
import { invoke } from './vendor/tauri-api/core.js';

const $ = (id) => document.getElementById(id);
const title = $('title');
const problemBox = $('problem');
const message = $('message');
const detail = $('detail');
const retryRow = $('retry-row');
const retry = $('retry');
const form = $('form');
const input = $('server');
const connectButton = $('connect');
const busy = $('busy');

/**
 * `status` and `problem` are the app's `Status` and `Problem` (src-tauri/src/main.rs):
 * the problem is the one the page opened with, or the last attempt's.
 */
function show(status, problem = status.problem) {
	title.textContent =
		problem?.title ?? (status.server ? 'coversync' : 'Connect to a coversync server');
	problemBox.hidden = !problem;
	message.textContent = problem?.message ?? '';
	detail.textContent = problem?.detail ?? '';
	detail.hidden = !problem?.detail;
	retryRow.hidden = !status.server;
	if (status.server && !input.value) input.value = status.server;
}

function setBusy(text) {
	busy.textContent = text ?? '';
	busy.hidden = !text;
	for (const control of [retry, input, connectButton]) control.disabled = Boolean(text);
}

async function connect(server) {
	setBusy(`Connecting to ${server ?? input.value}…`);
	try {
		// On success the app moves the window on to the server's UI.
		await invoke('connect', { server });
	} catch (err) {
		setBusy(null);
		const problem =
			typeof err === 'object' && err?.title
				? err
				: { title: 'Something went wrong', message: String(err) };
		show(await invoke('status'), problem);
	}
}

retry.addEventListener('click', () => connect(null));
form.addEventListener('submit', (event) => {
	event.preventDefault();
	connect(input.value.trim());
});

invoke('status').then((status) => {
	show(status);
	(status.server ? retry : input).focus();
});
