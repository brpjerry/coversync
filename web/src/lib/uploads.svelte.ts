// The uploads panel's list: uploads under way and just done, whoever sends
// them, with their progress, and the albums they became.
//
// This page sends its own, from drops and file pickers (upload-engine.ts).
// The desktop app (DSK-06) sends from its Rust side instead, and reports
// each of its uploads here with `uploads.add`, giving controls that call it
// back, then `update` with its progress a few times a second: the panel,
// the rates and the albums found work the same for both.
//
// Once an upload is done, the server scans it in, and its albums show up in
// the Inbox: the page tells this list when the Inbox reloads
// (`albumsLoaded`) and when a scan ends (`scanFinished`), and the server
// names the Inbox albums with files in the upload's folder, for the panel to
// show.

import { api, ApiError, sendChunk, type Album, type UploadsInfo } from './api';
import { notify } from './notify';
import { Uploader, type Phase, type Transport, type Upload } from './upload-engine';
import { fromEntries, fromFiles, type Entry, type Picked } from './upload-files';

export type { Phase } from './upload-engine';

export interface UploadItem {
	/** This page's key for it. */
	key: number;
	/** The folder it goes into, '' for loose files; once done, the one it went into. */
	folder: string;
	files: number;
	size: number;
	/** Bytes sent so far. */
	received: number;
	/** Bytes a second, over the last few seconds. */
	rate: number;
	phase: Phase;
	/** Why it failed, or the last failure while it retries. */
	error: string | null;
	/** When it tries again (ms since the epoch), while retrying. */
	retryAt: number | null;
	/** Files left out, as hidden or not a kind uploads take. */
	skipped: string[];
	/** Where it went on the server, once done. */
	path: string | null;
	/** The Inbox albums found in it, once scanned. */
	albums: string[];
	/** The Inbox has been scanned since it was done. */
	scanned: boolean;
}

/** What the panel's buttons do to an upload. */
export interface UploadControls {
	cancel(): void;
	/** A failed or cancelled upload again; one waiting to retry, now. */
	retry(): void;
	/** It was dismissed from the panel. */
	forget?(): void;
}

/** An upload as its sender first reports it. */
export type NewItem = Pick<UploadItem, 'folder' | 'files' | 'size'> & Partial<UploadItem>;

const ACTIVE: ReadonlySet<Phase> = new Set([
	'queued',
	'starting',
	'sending',
	'retrying',
	'finishing'
]);
export const isActive = (phase: Phase) => ACTIVE.has(phase);

/** How often progress and rates are brought up to date while uploads go. */
const TICK_MS = 250;
/** Rates are averaged over about this long. */
const RATE_MS = 2000;
/** A finished upload's albums are looked for this long, through later scans. */
const LOOK_MS = 30 * 60_000;

/** Problems with where uploads go, which Inbox setup fixes. */
export class UploadsUnavailable extends Error {}

/** The browser's way to the server: the API, with chunks by XMLHttpRequest for their progress. */
const transport: Transport = {
	start: (folder, files) => api.startUpload(folder, files),
	status: (id) => api.upload(id),
	put: sendChunk,
	finish: (id) => api.finishUpload(id),
	cancel: (id, keepalive) => api.cancelUpload(id, keepalive)
};

/**
 * How much is in flight at once. Over HTTP/2 or 3, requests share one
 * connection: many small files go fastest with many requests at once (twice
 * as fast with 32 as with 8, measured in Chromium), and large chunks with
 * about 8 (more is slower). Over HTTP/1.1, browsers open six connections to
 * a server, across its tabs: the page's event stream holds one, and its own
 * requests need another.
 */
function limits(chunk: number) {
	const [nav] = performance.getEntriesByType('navigation') as PerformanceNavigationTiming[];
	const protocol = nav?.nextHopProtocol ?? '';
	return protocol === 'h2' || protocol === 'h3'
		? { parallel: 32, budget: 8 * chunk }
		: { parallel: 4 };
}

class Uploads {
	items = $state<UploadItem[]>([]);
	/** Something is still under way. */
	busy = $derived(this.items.some((i) => isActive(i.phase)));

	#next = 0;
	#controls = new Map<number, UploadControls>();
	/** This page's own uploads, by item key, and the other way round. */
	#own = new Map<number, Upload>();
	#keys = new WeakMap<Upload, number>();
	/** The key of the upload the engine is adding: it reports the upload before it returns it. */
	#adding: number | null = null;
	#engine: Uploader | null = null;
	/** Items whose progress changed since the last tick. */
	#dirty = new Set<number>();
	#ticker: ReturnType<typeof setInterval> | undefined;
	/** For rates: each item's bytes and time at the last tick. */
	#last = new Map<number, { at: number; received: number }>();

	/** The Inbox's album ids as last loaded. */
	#shown = new Set<string>();
	/** When each was done. */
	#doneAt = new Map<number, number>();
	/** Counts each upload's album lookups, so only the latest answer lands. */
	#lookups = new Map<number, number>();
	/** Uploads done before the last scan ended, waiting for the Inbox to reload after it. */
	#scanning = new Set<number>();

	constructor() {
		if (typeof window === 'undefined') return;
		// Leaving the page stops its uploads: ask first.
		addEventListener('beforeunload', (e) => {
			if ([...this.#own.values()].some((u) => u.active)) e.preventDefault();
		});
		// And once it's going, remove what the server has of them, rather than
		// leave it there for a day. Firefox stops a page's script as its tab
		// navigates away or closes, at the next point the script checks for
		// that, which would be partway through this (measured in Firefox 160:
		// the cancel never went out). A throwaway listener takes the stop
		// first, a loop being such a point, so the one after it runs whole.
		addEventListener('pagehide', () => {
			for (let i = 0; i < 2; i++);
		});
		addEventListener('pagehide', () => this.#engine?.abandon());
		addEventListener('online', () => this.reconnected());
	}

	/** coversyncd can be reached again (or signed in to): uploads waiting to retry go now. */
	reconnected() {
		this.#engine?.retryAll();
	}

	/**
	 * Report an upload sent from elsewhere (the desktop app). Returns its key,
	 * and `update`, to report how it's going; the rate is worked out here.
	 */
	add(item: NewItem, controls: UploadControls) {
		const key = this.#next++;
		this.items.push({
			key,
			received: 0,
			rate: 0,
			phase: 'queued',
			error: null,
			retryAt: null,
			skipped: [],
			path: null,
			albums: [],
			scanned: false,
			...item
		});
		this.#controls.set(key, controls);
		this.#tick();
		return { key, update: (patch: Partial<UploadItem>) => this.#update(key, patch) };
	}

	cancel(key: number) {
		this.#controls.get(key)?.cancel();
	}

	retry(key: number) {
		this.#controls.get(key)?.retry();
	}

	/** Take an upload that's over off the panel. */
	dismiss(key: number) {
		const item = this.#item(key);
		if (!item || isActive(item.phase)) return;
		this.#controls.get(key)?.forget?.();
		for (const map of [this.#controls, this.#own, this.#last, this.#doneAt, this.#lookups])
			map.delete(key);
		this.#scanning.delete(key);
		this.items = this.items.filter((i) => i.key !== key);
	}

	/** Take every upload that's over off the panel. */
	clear() {
		for (const item of this.items.filter((i) => !isActive(i.phase))) this.dismiss(item.key);
	}

	/**
	 * Upload what was dropped: the drop's entries, from
	 * `DataTransferItem.webkitGetAsEntry()` while the drop was handled. Folders
	 * keep their structure. Returns the folders that had nothing to upload.
	 * Throws `UploadsUnavailable` when uploads can't go anywhere.
	 */
	async drop(entries: Entry[]): Promise<Picked[]> {
		const info = await this.#ready();
		return this.#send(await fromEntries(entries, new Set(info.extensions)));
	}

	/** Upload files from a file picker (one with `webkitdirectory` gives a folder); as `drop`. */
	async pick(files: Iterable<File>): Promise<Picked[]> {
		const info = await this.#ready();
		return this.#send(fromFiles(files, new Set(info.extensions)));
	}

	/** The Inbox's albums were loaded: find finished uploads' albums in it. */
	async albumsLoaded(inbox: Album[]) {
		this.#shown = new Set(inbox.map((a) => a.id));
		const scanned = [...this.#scanning];
		this.#scanning.clear();
		await Promise.all(this.items.map((item) => this.#look(item)));
		for (const key of scanned) this.#update(key, { scanned: true });
	}

	/**
	 * The albums a finished upload became, as the Inbox shows them: asked of
	 * the server for a while after it's done, and after that only those still
	 * in the Inbox kept (not imported or renamed since).
	 */
	async #look(item: UploadItem) {
		const done = this.#doneAt.get(item.key);
		if (item.phase !== 'done' || done === undefined) return;
		let albums = item.albums;
		if (Date.now() - done < LOOK_MS) {
			const n = (this.#lookups.get(item.key) ?? 0) + 1;
			this.#lookups.set(item.key, n);
			const found = await api.uploadAlbums(item.folder).then(
				(r) => r.albums,
				() => null
			);
			// A later lookup answered first, or it couldn't be had: leave it.
			if (this.#lookups.get(item.key) !== n || !found) return;
			albums = found;
		}
		const shown = albums.filter((id) => this.#shown.has(id));
		if (shown.join() !== item.albums.join()) this.#update(item.key, { albums: shown });
	}

	/** A scan ended: once the Inbox has reloaded after it, uploads done before it have been scanned. */
	scanFinished() {
		for (const item of this.items) if (item.phase === 'done') this.#scanning.add(item.key);
	}

	#item(key: number) {
		return this.items.find((i) => i.key === key);
	}

	#update(key: number, patch: Partial<UploadItem>) {
		const item = this.#item(key);
		if (!item) return;
		const was = item.phase;
		Object.assign(item, patch);
		if (item.phase === 'done' && was !== 'done') this.#done(item);
		if (isActive(item.phase)) this.#tick();
	}

	#done(item: UploadItem) {
		item.received = item.size;
		item.rate = 0;
		this.#doneAt.set(item.key, Date.now());
		// The scan may have shown its albums before the finish was answered.
		void this.#look(item);
		notify('Uploaded to the Inbox', item.folder || 'Upload', `upload-${item.key}`);
	}

	/** Bring progress and rates up to date every tick while something is under way. */
	#tick() {
		if (this.#ticker) return;
		this.#ticker = setInterval(() => {
			const now = performance.now();
			for (const key of this.#dirty) {
				const u = this.#own.get(key);
				const item = this.#item(key);
				if (u && item) item.received = u.received;
			}
			this.#dirty.clear();
			for (const item of this.items) {
				const last = this.#last.get(item.key);
				this.#last.set(item.key, { at: now, received: item.received });
				if (item.phase !== 'sending' && item.phase !== 'retrying') {
					if (item.rate) item.rate = 0;
					continue;
				}
				if (!last) continue;
				const dt = now - last.at;
				const instant = (Math.max(0, item.received - last.received) * 1000) / dt;
				// An average that forgets over about RATE_MS, from the first measure.
				item.rate = item.rate
					? item.rate + (instant - item.rate) * (1 - Math.exp(-dt / RATE_MS))
					: instant;
			}
			if (!this.busy) {
				clearInterval(this.#ticker);
				this.#ticker = undefined;
				this.#last.clear();
			}
		}, TICK_MS);
	}

	/** What uploads take, checking that they have somewhere to go. */
	async #ready(): Promise<UploadsInfo> {
		let info: UploadsInfo;
		try {
			info = await api.uploads();
		} catch (e) {
			throw new Error(e instanceof ApiError ? e.message : 'Could not reach coversyncd.');
		}
		const folder = info.folder;
		if (!folder)
			throw new UploadsUnavailable('Choose an Inbox folder in Inbox setup before uploading.');
		if (folder.exists ? !(folder.dir && folder.writable) : !folder.creatable)
			throw new UploadsUnavailable(
				`coversyncd can’t write to the uploads folder ${folder.path}. Choose a folder it can write to in Inbox setup.`
			);
		this.#engine ??= new Uploader(transport, {
			...limits(info.chunk_size),
			chunk: info.chunk_size,
			changed: (u, progress) => this.#changed(u, progress)
		});
		return info;
	}

	/** Start uploading each of `picked` that has files; returns those that have none. */
	#send(picked: Picked[]): Picked[] {
		const engine = this.#engine!;
		for (const p of picked.filter((p) => p.files.length)) {
			const key = this.#next;
			// The engine reports the upload as it's added: the item is there first.
			this.add(
				{
					folder: p.folder,
					files: p.files.length,
					size: p.files.reduce((n, f) => n + f.file.size, 0),
					skipped: p.skipped
				},
				{
					cancel: () => engine.cancel(this.#own.get(key)!),
					retry: () => engine.retry(this.#own.get(key)!),
					forget: () => engine.forget(this.#own.get(key)!)
				}
			);
			this.#adding = key;
			this.#own.set(key, engine.add(p));
			this.#adding = null;
		}
		return picked.filter((p) => !p.files.length);
	}

	#changed(u: Upload, progress: boolean) {
		let key = this.#keys.get(u);
		if (key === undefined && this.#adding !== null) this.#keys.set(u, (key = this.#adding));
		if (key === undefined) return;
		if (progress) {
			this.#dirty.add(key);
			return;
		}
		this.#update(key, {
			phase: u.phase,
			received: u.received,
			error: u.error,
			retryAt: u.retryAt,
			path: u.result?.path ?? null,
			folder: u.result?.folder ?? u.picked.folder
		});
	}
}

export const uploads = new Uploads();
