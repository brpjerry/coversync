// The fallback page: shown when no server is set, it can't be reached, or
// the page crashed. Once a server answers, the app moves the window on to
// its UI.

// The app's commands, through Tauri's IPC or the Linux app's bridge (see bridge.js).
import { invoke } from './bridge.js';

const $ = (id) => document.getElementById(id);
const title = $('title');
const problemBox = $('problem');
const address = $('address');
const message = $('message');
const detail = $('detail');
const retryRow = $('retry-row');
const retry = $('retry');
const form = $('form');
const input = $('server');
const connectButton = $('connect');
const busy = $('busy');

// The server in use, which retrying opens: the field may hold another address.
let current = null;

/**
 * `status` is the app's `Status` (each app's main.rs), and `problem` its `Problem`
 * (crates/coversync-desktop-core/src/shell.rs): the one the page opened with, or the last
 * attempt's. A problem
 * names the address it's about, which goes in the field: a refused new address, not the
 * server still in use.
 */
function show(status, problem = status.problem) {
	title.textContent =
		problem?.title ?? (status.server ? 'coversync' : 'Connect to a coversync server');
	problemBox.hidden = !problem;
	address.textContent = problem?.server ?? '';
	address.hidden = !problem?.server;
	message.textContent = problem?.message ?? '';
	detail.textContent = problem?.detail ?? '';
	detail.hidden = !problem?.detail;
	current = status.server ?? null;
	retryRow.hidden = !status.server;
	// Retrying opens the server in use, which isn't the one refused.
	const other = problem?.server && status.server && problem.server !== status.server;
	retry.textContent = other ? `Back to ${status.server}` : 'Try again';
	const shown = problem?.server ?? status.server;
	if (shown) input.value = shown;
}

function setBusy(text) {
	busy.textContent = text ?? '';
	busy.hidden = !text;
	for (const control of [retry, input, connectButton]) control.disabled = Boolean(text);
}

async function connect(server) {
	setBusy(`Connecting to ${server ?? current}…`);
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
