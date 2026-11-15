// The app's settings page: what's kept on this computer, not on the server. The window shows it
// over the page it was opened from, which stays as it is. So far it holds the sync settings:
// whether the sync agent runs, and the folder the copy of the Library is kept in.

// The app's commands (see bridge.js).
import { invoke } from './bridge.js';

const $ = (id) => document.getElementById(id);
const closeButton = $('close');
const notice = $('notice');
const folder = $('folder');
const chooseButton = $('choose');
const server = $('server');
const toggle = $('toggle');
const status = $('status');
const busy = $('busy');

/** How often the agent's status is read while the page is up. */
const REFRESH_MS = 2000;
/**
 * How long the controls stay still after the page comes up. A server's page can open it at any
 * moment (that's its one command): a click meant for that page mustn't land on a switch here.
 */
const SETTLE_MS = 600;

// A command is under way, or the page has only just come up: the controls wait.
let working = true;

/** A server's address without its scheme, as people say it. */
const plain = (url) => url.replace(/^https?:\/\//, '').replace(/\/$/, '');

/**
 * Shows `overview`, the app's `Overview` (crates/coversync-desktop-core/src/sync.rs).
 * Everything is set as text: nothing here is markup.
 */
function show(overview) {
	folder.textContent = overview.folder ?? 'Not chosen';
	const elsewhere =
		overview.on && overview.syncs_with && overview.server && overview.syncs_with !== overview.server;
	server.textContent = elsewhere
		? `Now with ${plain(overview.syncs_with)}. Turning it on here moves it to ${plain(overview.server)}.`
		: overview.server
			? `With ${plain(overview.server)}`
			: overview.syncs_with
				? `With ${plain(overview.syncs_with)}`
				: 'No server is open';
	// On, for this page, is on with the server the window shows.
	const on = overview.on && !elsewhere;
	toggle.setAttribute('aria-checked', String(on));

	// What stands in the way of turning it on, most basic first.
	let blocked = null;
	if (!overview.installed)
		blocked = "The sync agent isn't installed on this computer. It comes in the coversync-agent package.";
	else if (overview.unpaired)
		blocked =
			"The server no longer accepts this computer, so sync has stopped. Pair it again (Add device, in the server's Sync setup), then turn sync on.";
	else if (!on && overview.server && !overview.paired)
		blocked =
			"This computer isn't paired with the server yet. Pair it first: Add device, in the server's Sync setup.";
	notice.hidden = blocked === null;
	notice.textContent = blocked ?? '';

	const ready = overview.installed && (on || (overview.server && overview.paired && overview.folder));
	toggle.disabled = working || !ready;
	chooseButton.disabled = working;
	if (on && overview.running) status.textContent = overview.status || 'Starting.';
	else if (on) status.textContent = overview.unpaired ? '' : 'Sync is on, and not running right now.';
	else if (elsewhere) status.textContent = overview.status;
	else if (!overview.server) status.textContent = 'Open a server to turn sync on.';
	else if (!overview.folder) status.textContent = 'Choose a folder to turn sync on.';
	else status.textContent = 'Sync is off.';
}

/** Says what went wrong: `problem` is the app's `Problem`. */
function fail(problem) {
	notice.hidden = false;
	notice.textContent = [problem?.title, problem?.message].filter(Boolean).join(': ');
}

async function refresh() {
	if (working) return;
	try {
		const overview = await invoke('settings');
		if (!working) show(overview);
	} catch (problem) {
		fail(problem);
	}
}

// What there is to see shows at once; only the controls wait.
invoke('settings')
	.then(show)
	.catch(fail);

/** Runs a command that answers with the overview, keeping the controls still meanwhile. */
async function run(cmd, args, doing) {
	if (working) return;
	working = true;
	toggle.disabled = chooseButton.disabled = true;
	busy.hidden = false;
	busy.textContent = doing;
	let overview = null;
	let problem = null;
	try {
		overview = await invoke(cmd, args);
	} catch (err) {
		problem = err;
	}
	working = false;
	busy.hidden = true;
	if (overview) show(overview);
	else {
		await refresh();
		fail(problem);
	}
}

toggle.addEventListener('click', () => {
	const on = toggle.getAttribute('aria-checked') !== 'true';
	run('sync_turn', { on }, on ? 'Turning sync on…' : 'Turning sync off…');
});
chooseButton.addEventListener('click', () => run('sync_choose_folder', {}, 'Choosing a folder…'));
closeButton.addEventListener('click', () => invoke('close_settings').catch(() => {}));
document.addEventListener('keydown', (event) => {
	if (event.key === 'Escape') invoke('close_settings').catch(() => {});
});

toggle.disabled = chooseButton.disabled = true;
setTimeout(() => {
	working = false;
	refresh();
}, SETTLE_MS);
setInterval(refresh, REFRESH_MS);
