// Importing several Inbox albums in a row: each still gets its own import
// dialog, one after another, and each import runs in the background while
// the next album is reviewed.

import { api, ApiError, type Album, type ImportEdits, type ImportPlan } from './api';

export type BatchStatus = 'waiting' | 'importing' | 'imported' | 'skipped' | 'failed';

/** The suggestions' quick choices: Tick all, Only empty tags, None. */
export type TickPreset = 'all' | 'fill' | 'none';

/** Which suggestion rows a quick choice ticks. */
export const presetPick: Record<TickPreset, (row: { fill: boolean }) => boolean> = {
	all: () => true,
	fill: (row) => row.fill,
	none: () => false
};

const message = (e: unknown, fallback: string) => (e instanceof ApiError ? e.message : fallback);

/** What an album's import dialog needs from the batch it's in. */
export interface BatchControls {
	batch: ImportBatch;
	item: BatchItem;
	/** Go to the album at this place in the batch. */
	onjump: (index: number) => void;
	onskip: () => void;
	onstop: () => void;
}

export class BatchItem {
	readonly album: Album;
	/** Its plan as it was before its dialog opened (the dialog re-plans as it's edited); null until it's made. */
	plan = $state<ImportPlan | null>(null);
	status = $state<BatchStatus>('waiting');
	/** Why its plan or its import failed. */
	error = $state<string | null>(null);
	/** Where it went in the Library, and its id there, once imported. */
	dest = $state<string | null>(null);
	libraryId = $state<string | null>(null);
	/** Its Smart suggestions lookup, fetched ahead of its dialog. */
	lookup = $state<'idle' | 'running' | 'done'>('idle');
	/** Shown once: its dialog is kept, hidden, while the batch lasts, with what was done in it. */
	opened = $state(false);
	/**
	 * What its dialog, once shown, says it still needs, as it's edited
	 * there; null when it's ready. Until then the strip goes by its plan.
	 */
	needs = $state<string | null | undefined>(undefined);
	/** Its first plan, while it's on its way: asked for once. */
	pending: Promise<ImportPlan | null> | null = null;

	constructor(album: Album) {
		this.album = album;
	}

	/** Not imported, nor being imported. */
	get open() {
		return this.status !== 'importing' && this.status !== 'imported';
	}
}

export class ImportBatch {
	readonly items: BatchItem[];
	/** Smart suggestions were on when it started: its albums are looked up ahead. */
	readonly suggest: boolean;
	index = $state(0);
	/** The collection last picked, for the next album without one; null until one is. */
	collection = $state<string | null>(null);
	/** The suggestions' quick choice last used, for the next album's. */
	preset = $state<TickPreset | undefined>();
	/** Stopped, or every album was imported or skipped: the summary shows. */
	ended = $state(false);
	/** Its summary was closed while imports still ran: a toast tells how they went. */
	dismissed = $state(false);
	/** Its progress toast, while imports run. */
	toast: number | null = null;
	/** The toast saying how its imports went is out. */
	announced = false;
	/** Let go of: plans and lookups fetched ahead stop. */
	#closed = false;
	#fetching = false;

	constructor(albums: Album[], suggest: boolean) {
		this.items = albums.map((a) => new BatchItem(a));
		this.suggest = suggest;
	}

	get current(): BatchItem {
		return this.items[this.index];
	}

	count(status: BatchStatus) {
		return this.items.filter((i) => i.status === status).length;
	}

	/** Albums still to decide on. */
	get waiting() {
		return this.count('waiting');
	}

	/** The next album still waiting after `from`, then from the start; null when none is. */
	nextWaiting(from = this.index): number | null {
		const n = this.items.length;
		for (let k = 1; k <= n; k++) {
			const i = (from + k) % n;
			if (this.items[i].status === 'waiting') return i;
		}
		return null;
	}

	/** The current album is the only one still waiting: its import ends the batch. */
	get last() {
		return this.items.every((i) => i === this.current || i.status !== 'waiting');
	}

	/**
	 * Its plan before any edits: the one it has, or the one on its way (asked
	 * for once, whoever asks first). One made since, with edits, isn't replaced.
	 */
	firstPlan(item: BatchItem): Promise<ImportPlan | null> {
		if (item.plan) return Promise.resolve(item.plan);
		// Asked again: its earlier failure no longer says how it stands. One
		// listed as failed keeps its reason until it's opened again.
		if (!item.pending && item.status === 'waiting') item.error = null;
		item.pending ??= api.importPlan(item.album.id, {}).then(
			(plan) => {
				if (!item.plan) {
					item.plan = plan;
					// It's been checked after all.
					if (item.status === 'waiting') item.error = null;
				}
				return item.plan;
			},
			(e) => {
				// One made since, with edits, stands.
				if (item.plan) return item.plan;
				item.error = message(e, 'Could not check the album');
				return null;
			}
		);
		return item.pending.finally(() => (item.pending = null));
	}

	#planning = false;

	/**
	 * Plans for every album, a few at a time, so the strip can say what each
	 * needs. Asked again while it's at it (a Retry, say), it carries on as it is.
	 */
	async loadPlans(limit = 4) {
		if (this.#planning) return;
		this.#planning = true;
		let next = 0;
		const work = async () => {
			while (next < this.items.length && !this.#closed && !this.ended) {
				await this.firstPlan(this.items[next++]);
				// Lookups wait for an album's plan to say it uses them.
				void this.prefetch();
			}
		};
		try {
			await Promise.all(Array.from({ length: Math.min(limit, this.items.length) }, work));
		} finally {
			this.#planning = false;
		}
	}

	/**
	 * Fetch the albums' lookups ahead, one at a time: the current album's
	 * first, then the next ones in order. AcoustID and MusicBrainz are rate
	 * limited for every lookup at once, so more at a time would only slow
	 * the album being reviewed. The daemon keeps each result for its dialog.
	 */
	async prefetch(sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))) {
		if (!this.suggest || this.#fetching) return;
		this.#fetching = true;
		try {
			for (let item = this.#nextLookup(); item; item = this.#nextLookup()) {
				item.lookup = 'running';
				try {
					let s = await api.identify(item.album.id);
					while (s.state === 'running' && !this.#closed && !this.ended) {
						await sleep(500);
						s = await api.identifyStatus(item.album.id);
					}
				} catch {
					// Its dialog asks again, and says what went wrong.
				}
				item.lookup = 'done';
			}
		} finally {
			this.#fetching = false;
		}
	}

	/**
	 * The next album to look up: the current one first, then the next in
	 * order. One whose plan isn't back yet is waited for (its plan wakes the
	 * worker) rather than passed over; one whose plan says it doesn't use
	 * suggestions, or failed, is.
	 */
	#nextLookup(): BatchItem | undefined {
		if (this.#closed || this.ended) return;
		const n = this.items.length;
		for (let k = 0; k < n; k++) {
			const item = this.items[(this.index + k) % n];
			// An album whose dialog is open looks itself up, its own way (another
			// release, say): a default lookup now would replace it.
			if (item.lookup !== 'idle' || item.status !== 'waiting' || item.opened) continue;
			if (!item.plan && !item.error) return;
			if (item.plan?.suggest) return item;
		}
	}

	/** Import `item`; resolves once the daemon has done it, or failed to. */
	async import(item: BatchItem, edits: ImportEdits, art?: string, musicbrainzArt?: string) {
		item.status = 'importing';
		item.error = null;
		try {
			const r = await api.importAlbum(item.album.id, edits, art, musicbrainzArt);
			item.dest = r.dest_dir;
			item.libraryId = r.album_id;
			item.status = 'imported';
		} catch (e) {
			item.error = message(e, 'The import failed');
			item.status = 'failed';
		}
	}

	/** Mark the current album skipped; it stays in the Inbox. */
	skip() {
		if (this.current.status === 'waiting' || this.current.status === 'failed') {
			this.current.status = 'skipped';
		}
	}

	close() {
		this.#closed = true;
	}
}
