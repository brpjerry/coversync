// The fallback page: shown when no server is set or it can't be reached.
// Once a server answers, the app moves the window on to its UI.

const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const title = $('title');
const problem = $('problem');
const retryRow = $('retry-row');
const retry = $('retry');
const form = $('form');
const input = $('server');
const connectButton = $('connect');
const busy = $('busy');

/** `status` is the app's (see `status` in src-tauri/src/main.rs). */
function show(status, message = status.problem) {
	title.textContent = status.problem
		? "Can't reach the server"
		: status.server
			? 'coversync'
			: 'Connect to a coversync server';
	problem.textContent = message ?? '';
	problem.hidden = !message;
	retryRow.hidden = !status.server;
	if (status.server && !input.value) input.value = status.server;
}

function setBusy(message) {
	busy.textContent = message ?? '';
	busy.hidden = !message;
	for (const control of [retry, input, connectButton]) control.disabled = Boolean(message);
}

async function connect(server) {
	setBusy(`Connecting to ${server ?? input.value}…`);
	try {
		// On success the app moves the window on to the server's UI.
		await invoke('connect', { server });
	} catch (err) {
		setBusy(null);
		show(await invoke('status'), String(err));
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
