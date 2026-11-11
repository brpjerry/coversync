// The fallback page: shown when no server is set, it can't be reached, the
// page crashed, or the server took this computer off its devices. Here a
// server is chosen, and the computer paired with it. Once a server answers,
// the app moves the window on to its UI.

// The app's commands (see bridge.js).
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
const pairForm = $('pair-form');
const pairServer = $('pair-server');
const code = $('code');
const pairButton = $('pair');
const passwordButton = $('password');
const otherButton = $('other');
const forgetRow = $('forget-row');
const forgetButton = $('forget');
const busy = $('busy');

// The server in use, which retrying opens: the field may hold another address.
let current = null;
// The server the pairing form is for, while it shows.
let pairing = null;

/**
 * `status` is the app's `Status` (../src/app.rs), and `problem` its `Problem`
 * (crates/coversync-desktop-core/src/shell.rs): the one the page opened with, or the last
 * attempt's. A problem
 * names the address it's about, which goes in the field: a refused new address, not the
 * server still in use. One that says `pair` (the server removed this computer) comes with
 * the pairing form for that address.
 */
function show(status, problem = status.problem) {
	current = status.server ?? null;
	if (problem?.pair && problem.server) pairing = problem.server;
	title.textContent =
		problem?.title ??
		(pairing
			? 'Pair this computer'
			: status.server
				? 'coversync'
				: 'Connect to a coversync server');
	problemBox.hidden = !problem;
	// The pairing form names its server itself.
	const named = pairing ? null : problem?.server;
	address.textContent = named ?? '';
	address.hidden = !named;
	message.textContent = problem?.message ?? '';
	detail.textContent = problem?.detail ?? '';
	detail.hidden = !problem?.detail;

	form.hidden = Boolean(pairing);
	pairForm.hidden = !pairing;
	pairServer.textContent = pairing ?? '';
	retryRow.hidden = Boolean(pairing) || !status.server;
	forgetRow.hidden = Boolean(pairing) || !status.server;
	// Retrying opens the server in use, which isn't the one refused.
	const other = problem?.server && status.server && problem.server !== status.server;
	retry.textContent = other ? `Back to ${status.server}` : 'Try again';
	const shown = problem?.server ?? status.server;
	if (shown && !pairing) input.value = shown;
}

function setBusy(text) {
	busy.textContent = text ?? '';
	busy.hidden = !text;
	const controls = [
		retry,
		input,
		connectButton,
		code,
		pairButton,
		passwordButton,
		otherButton,
		forgetButton
	];
	for (const control of controls) control.disabled = Boolean(text);
}

/** Runs a command, saying `doing` meanwhile; what went wrong is shown, and `false` returned. */
async function attempt(doing, cmd, args) {
	setBusy(doing);
	try {
		return { value: await invoke(cmd, args) };
	} catch (err) {
		const problem =
			typeof err === 'object' && err?.title
				? err
				: { title: 'Something went wrong', message: String(err) };
		show(await invoke('status'), problem);
		return false;
	} finally {
		setBusy(null);
	}
}

/** Opens `server` (or, with null, the one in use again), signed in as it is. */
async function connect(server) {
	// On success the app moves the window on to the server's UI.
	const done = await attempt(`Connecting to ${server ?? current}…`, 'connect', { server });
	if (done) setBusy(`Opening ${server ?? current}…`);
}

/** Looks at a new address: a server this computer is paired with opens; another offers pairing. */
async function choose(server) {
	const checked = await attempt(`Checking ${server}…`, 'check', { server });
	if (!checked) return;
	const { server: url, paired } = checked.value;
	if (paired) return connect(url);
	pairing = url;
	code.value = '';
	show(await invoke('status'), null);
	code.focus();
}

retry.addEventListener('click', () => connect(null));
form.addEventListener('submit', (event) => {
	event.preventDefault();
	choose(input.value.trim());
});
pairForm.addEventListener('submit', async (event) => {
	event.preventDefault();
	const server = pairing;
	// On success the app moves the window on to the server's UI, signed in as this computer.
	const done = await attempt(`Pairing with ${server}…`, 'pair', { server, code: code.value });
	if (done) setBusy(`Opening ${server}…`);
	else code.select();
});
passwordButton.addEventListener('click', () => connect(pairing));
otherButton.addEventListener('click', async () => {
	input.value = pairing ?? '';
	pairing = null;
	show(await invoke('status'), null);
	input.select();
});
forgetButton.addEventListener('click', async () => {
	const server = current;
	const sure = confirm(
		`Forget ${server}?\n\nThis computer's sign-in for it is removed, and what the window kept ` +
			'of it. If this computer is paired with it, remove it from the server’s devices too.'
	);
	if (!sure) return;
	// Whether or not all of it could be forgotten, the page shows what is left.
	if (await attempt(`Forgetting ${server}…`, 'forget', { server })) {
		input.value = '';
		show(await invoke('status'));
	}
	input.focus();
});

invoke('status').then((status) => {
	show(status);
	(pairing ? code : status.server ? retry : input).focus();
});
