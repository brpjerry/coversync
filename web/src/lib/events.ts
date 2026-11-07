// Change notices the daemon pushes from /api/events as server-sent events,
// so a page hears about scans, imports, tag edits and saved setups made
// anywhere, by any client, as they happen.

import { BASE, type Collection, type ScanProgress } from './api';

/** Each notice, by event name, with the fields it carries. */
export interface Notices {
	/** A scan's progress, about once a second while it runs. */
	scan: ScanProgress;
	/** A scan ended: the Inbox and Library may have changed. */
	scan_finished: ScanProgress;
	/** An Inbox album was imported: `album_id` is its id in the Library. */
	imported: { album_id: string; inbox_id: string };
	/** An album's tags were edited. `previous_id` is its id before, if the edit changed it. */
	album_changed: { id: string; previous_id: string | null; collection: Collection };
	/** A setup screen was saved. `inbox` also means its hidden files were shown again. */
	setup_saved: { setup: 'library' | 'inbox' | 'appearance' | 'sync' };
	/** A Smart suggestions lookup ended. */
	lookup_finished: { album_id: string; state: 'done' | 'failed' };
	/** Notices were missed, so everything should be reloaded. */
	resync: Record<string, never>;
}

export type NoticeHandlers = { [K in keyof Notices]?: (notice: Notices[K]) => void };

const NAMES = [
	'scan',
	'scan_finished',
	'imported',
	'album_changed',
	'setup_saved',
	'lookup_finished',
	'resync'
] as const satisfies readonly (keyof Notices)[];

/** The daemon pings a quiet stream every 20 s; one silent this long has died unnoticed. */
const SILENT_MS = 50_000;
/**
 * Retries wait 1 s, then twice as long each time up to 10 s: back soon after
 * a restart, and one try in 10 s while the daemon stays away.
 */
const RETRY_MIN_MS = 1000;
const RETRY_MAX_MS = 10_000;

/**
 * Listen for notices until the returned function is called. When the stream
 * drops (the daemon restarted, the network went), it's opened again after a
 * growing delay. `onlive` hears each time the stream comes up or goes down,
 * and `resync` is called whenever it comes back up, as notices sent while it
 * was down are lost.
 */
export function listen(handlers: NoticeHandlers, onlive: (live: boolean) => void): () => void {
	let source: EventSource | null = null;
	let retry: ReturnType<typeof setTimeout> | undefined;
	let watchdog: ReturnType<typeof setTimeout> | undefined;
	let delay = RETRY_MIN_MS;
	/** It has been down since the page loaded, so notices may have been missed. */
	let dropped = false;
	let stopped = false;

	function watch() {
		clearTimeout(watchdog);
		watchdog = setTimeout(drop, SILENT_MS);
	}

	/** Something arrived: the stream works, so the next drop retries quickly again. */
	function alive() {
		delay = RETRY_MIN_MS;
		watch();
	}

	function on<K extends keyof Notices>(es: EventSource, name: K) {
		es.addEventListener(name, (e) => {
			alive();
			handlers[name]?.(JSON.parse(e.data) as Notices[K]);
		});
	}

	function connect() {
		const es = (source = new EventSource(BASE + '/api/events'));
		es.onopen = () => {
			watch();
			onlive(true);
			if (dropped) handlers.resync?.({});
		};
		// The browser would retry by itself, but not after an error response
		// (a proxy's 502 while the daemon restarts), and without backing off.
		es.onerror = drop;
		es.addEventListener('ping', alive);
		for (const name of NAMES) on(es, name);
	}

	function drop() {
		source?.close();
		source = null;
		clearTimeout(watchdog);
		clearTimeout(retry);
		if (stopped) return;
		dropped = true;
		onlive(false);
		retry = setTimeout(connect, delay * (0.75 + Math.random() / 2));
		delay = Math.min(delay * 2, RETRY_MAX_MS);
	}

	connect();
	return () => {
		stopped = true;
		drop();
	};
}
