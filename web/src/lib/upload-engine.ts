// Uploads sent from this page: picked files go to the server in chunks,
// through the upload API (`POST /api/uploads`, then `PUT` each chunk at its
// offset, then finish), and an upload carries on from what the server has
// after a dropped connection or a restart of coversyncd, for as long as the
// page is open. Resuming after a reload is the desktop app's job: a page
// can't read the files again then.
//
// Speed (the speed rule: only the network, the disks or the CPUs may hold
// an upload back): one pool of chunks in flight across all uploads, so the
// link stays full from one album to the next. Once an upload has no chunks
// left to hand out, the next one starts while its last chunks are still on
// their way, and its finishing (the server's flush and move) overlaps the
// next one's sending. Chunks are large (the server's size, 16 MiB), and the
// browser reads each slice from disk as it sends it. What's in flight is
// bounded twice, by requests and by bytes: over HTTP/2 many small files go
// fastest with many requests at once, and large chunks with a few.

import type { FinishedUpload, ServerUpload } from './api';
import type { Picked } from './upload-files';

export type Phase =
	/** Waiting for the uploads ahead of it. */
	| 'queued'
	| 'starting'
	| 'sending'
	/** The connection dropped, or coversyncd is away: trying again at `retryAt`. */
	| 'retrying'
	/** All sent: the server moves it into the uploads folder. */
	| 'finishing'
	| 'done'
	| 'failed'
	| 'cancelled';

/** How the engine reaches the server. Errors carry the HTTP `status`, 0 when no answer came. */
export interface Transport {
	start(folder: string, files: { path: string; size: number }[]): Promise<ServerUpload>;
	status(id: string): Promise<ServerUpload>;
	put(
		id: string,
		index: number,
		offset: number,
		body: Blob,
		progress: (sent: number) => void,
		signal: AbortSignal
	): Promise<void>;
	finish(id: string): Promise<FinishedUpload>;
	/** `keepalive`: sent even as the page goes away. */
	cancel(id: string, keepalive?: boolean): Promise<void>;
}

export interface Options {
	/** Chunks in flight at once, across all uploads. */
	parallel: number;
	/** Bytes in flight at once, beyond the first chunk. */
	budget?: number;
	/** Bytes per chunk. */
	chunk: number;
	/** Something about `upload` changed; `progress`: only how far it got. */
	changed: (upload: Upload, progress: boolean) => void;
	/** Waits before each try after a failure in a row, in ms; the last one repeats. */
	delays?: number[];
}

interface Chunk {
	index: number;
	offset: number;
	length: number;
}

interface Flight {
	/** Bytes of the chunk handed to the network so far. */
	sent: number;
	stop: AbortController;
}

/** Retried failures that aren't the connection's: after this many in a row, the upload fails. */
const SERVER_FAILURES = 3;
const DELAYS = [1000, 2000, 4000, 8000, 15000];

const statusOf = (e: unknown) =>
	typeof (e as { status?: unknown })?.status === 'number' ? (e as { status: number }).status : 0;
const messageOf = (e: unknown) =>
	e instanceof Error && e.message ? e.message : 'Something went wrong on the way to coversyncd.';

/**
 * Whether trying again may help: no answer at all, a proxy that couldn't
 * reach coversyncd, the session ended (it waits for signing back in), or an
 * error on the server.
 */
function transient(status: number) {
	return status === 0 || status === 401 || status === 408 || status === 429 || status >= 500;
}
/** Failures a connection or a restart explains, tried again for as long as the page is open. */
const unlimited = (status: number) =>
	status === 0 || status === 401 || status === 502 || status === 503 || status === 504;

export class Upload {
	phase: Phase = 'queued';
	/** The server's id for it, once started. */
	id: string | null = null;
	/** Why it failed, or the last failure while it retries. */
	error: string | null = null;
	/** When it tries again (ms since the epoch), while retrying. */
	retryAt: number | null = null;
	result: FinishedUpload | null = null;
	readonly size: number;
	/** Bytes the server has said it has, or acknowledged since. */
	confirmed = 0;
	pending: Chunk[] = [];
	inflight = new Map<Chunk, Flight>();
	/** Failures in a row, for the waits between tries. */
	failures = 0;
	/** What failed, to try again: starting, sending (after asking how far it got) or finishing. */
	step: 'start' | 'resume' | 'finish' = 'start';
	/**
	 * The most it has been shown to have sent, so the progress shown doesn't
	 * go back when bytes on their way are lost to a dropped connection.
	 */
	held = 0;
	/** Asking the server how far it got, to carry on. */
	asking = false;
	/** The server lost bytes it had (a 409 at finishing): show what it has once it says. */
	recount = false;
	timer: ReturnType<typeof setTimeout> | undefined;

	constructor(readonly picked: Picked) {
		this.size = picked.files.reduce((total, f) => total + f.file.size, 0);
	}

	/** Bytes sent: those the server has, and those of chunks on their way. */
	get received(): number {
		if (this.phase === 'done') return this.size;
		if (this.phase === 'retrying') return this.held;
		let n = this.confirmed;
		for (const f of this.inflight.values()) n += f.sent;
		return Math.max(this.held, Math.min(n, this.size));
	}

	/**
	 * Whether Cancel can stop it: not once it's finishing, nor while a finish
	 * whose answer was lost is asked again, as it may have gone through.
	 */
	get cancellable() {
		return (
			this.active &&
			this.phase !== 'finishing' &&
			!(this.phase === 'retrying' && this.step === 'finish')
		);
	}

	get active() {
		return (
			this.phase === 'queued' ||
			this.phase === 'starting' ||
			this.phase === 'sending' ||
			this.phase === 'retrying' ||
			this.phase === 'finishing'
		);
	}
}

export class Uploader {
	#uploads: Upload[] = [];
	/** Chunks in flight. */
	#busy = 0;
	/** Their bytes. */
	#load = 0;
	/** Uploads being started. */
	#starting = 0;
	/** Uploads being cancelled on the server, asked again while it can't be reached. */
	#deleting = new Set<string>();

	constructor(
		private transport: Transport,
		private options: Options
	) {}

	/** Queue an upload of `picked`. */
	add(picked: Picked): Upload {
		const upload = new Upload(picked);
		this.#uploads.push(upload);
		this.#changed(upload);
		this.#pump();
		return upload;
	}

	/** Stop an upload, and remove what the server has of it. Finishing can't be stopped. */
	cancel(u: Upload) {
		if (!u.cancellable) return;
		const id = u.id;
		// Being started: the start's answer is dropped when it comes.
		this.#halt(u, 'cancelled');
		if (id) this.#drop(id);
		this.#changed(u);
		this.#pump();
	}

	/** Try a failed or cancelled upload again, from the start; or one waiting to retry, now. */
	retry(u: Upload) {
		if (u.phase === 'retrying') return void this.#again(u);
		if (u.phase !== 'failed' && u.phase !== 'cancelled') return;
		u.phase = 'queued';
		u.id = null;
		u.error = null;
		u.confirmed = 0;
		u.failures = 0;
		u.step = 'start';
		this.#changed(u);
		this.#pump();
	}

	/** The connection is back: every upload waiting to retry tries now. */
	retryAll() {
		for (const u of this.#uploads) if (u.phase === 'retrying') this.#again(u);
	}

	/** Forget an upload that's over. */
	forget(u: Upload) {
		if (!u.active) this.#uploads = this.#uploads.filter((x) => x !== u);
	}

	/**
	 * The page is going away, and its files with it: cancel what's under way,
	 * so the server doesn't keep half an upload until it expires.
	 */
	abandon() {
		for (const id of this.#deleting) this.transport.cancel(id, true).catch(() => {});
		const stopped = this.#uploads.filter((u) => u.cancellable);
		for (const u of stopped) {
			const id = u.id;
			this.#halt(u, 'cancelled');
			if (id) this.transport.cancel(id, true).catch(() => {});
		}
		// Once the cancels are out: a page kept in the back-forward cache shows them when it's back.
		for (const u of stopped) this.#changed(u);
	}

	#changed(u: Upload, progress = false) {
		this.options.changed(u, progress);
	}

	/**
	 * Hand out chunks while there's room, and start the next upload once
	 * none are left to hand out.
	 */
	#pump() {
		while (this.#busy < this.options.parallel) {
			const u = this.#uploads.find((x) => x.phase === 'sending' && x.pending.length);
			if (!u) break;
			if (this.#busy && this.#load + u.pending[0].length > (this.options.budget ?? Infinity)) break;
			void this.#send(u, u.pending.shift()!);
		}
		// One at a time, and not while one waits to retry: coversyncd is away.
		const waiting = (x: Upload) =>
			(x.phase === 'sending' && x.pending.length) || x.phase === 'retrying';
		if (this.#starting || this.#uploads.some(waiting)) return;
		const next = this.#uploads.find((x) => x.phase === 'queued');
		if (next) void this.#start(next);
	}

	async #start(u: Upload) {
		this.#starting++;
		u.phase = 'starting';
		this.#changed(u);
		const files = u.picked.files.map((f) => ({ path: f.path, size: f.file.size }));
		try {
			const started = await this.transport.start(u.picked.folder, files);
			if (u.phase !== 'starting') {
				// Cancelled meanwhile.
				this.#drop(started.id);
				return;
			}
			u.id = started.id;
			this.#plan(u, started);
			u.phase = 'sending';
		} catch (e) {
			if (u.phase === 'starting') this.#trouble(u, 'start', e);
		} finally {
			this.#starting--;
			this.#changed(u);
			this.#settle(u);
			this.#pump();
		}
	}

	/** The chunks the server lacks, from each file's ranges. */
	#plan(u: Upload, server: ServerUpload) {
		const chunk = this.options.chunk;
		u.pending = [];
		u.confirmed = 0;
		for (const f of server.files) {
			let at = 0;
			const gaps: [number, number][] = [];
			for (const [start, end] of f.ranges) {
				if (start > at) gaps.push([at, start]);
				at = Math.max(at, end);
				u.confirmed += end - start;
			}
			if (at < f.size) gaps.push([at, f.size]);
			for (const [start, end] of gaps)
				for (let offset = start; offset < end; offset += chunk)
					u.pending.push({ index: f.index, offset, length: Math.min(chunk, end - offset) });
		}
	}

	async #send(u: Upload, chunk: Chunk) {
		this.#busy++;
		this.#load += chunk.length;
		const flight: Flight = { sent: 0, stop: new AbortController() };
		u.inflight.set(chunk, flight);
		const file = u.picked.files[chunk.index].file;
		const body = file.slice(chunk.offset, chunk.offset + chunk.length);
		try {
			await this.transport.put(
				u.id!,
				chunk.index,
				chunk.offset,
				body,
				(sent) => {
					flight.sent = Math.min(sent, chunk.length);
					this.#changed(u, true);
				},
				flight.stop.signal
			);
			if (u.inflight.get(chunk) === flight) {
				u.inflight.delete(chunk);
				u.confirmed += chunk.length;
				u.failures = 0;
				this.#changed(u, true);
			}
		} catch (e) {
			// Stopped on purpose (cancelled, or another chunk's failure): nothing to say.
			if (u.inflight.get(chunk) === flight) {
				u.held = u.received;
				u.inflight.delete(chunk);
				if (u.phase === 'sending') await this.#troubleSending(u, chunk, e);
			}
		} finally {
			this.#busy--;
			this.#load -= chunk.length;
		}
		this.#settle(u);
		this.#pump();
	}

	/**
	 * A chunk failed. The whole upload retries, asking the server how far it
	 * got, unless the browser can't read the file any more (moved, changed, on
	 * a disk that went): trying again won't help that.
	 */
	async #troubleSending(u: Upload, chunk: Chunk, e: unknown) {
		this.#trouble(u, 'resume', e);
		if (u.phase !== 'retrying') return;
		const file = u.picked.files[chunk.index];
		const readable = await file.file
			.slice(chunk.offset, chunk.offset + 1)
			.arrayBuffer()
			.then((b) => b.byteLength === 1)
			.catch(() => false);
		if (!readable && u.phase === 'retrying')
			this.#fail(u, `Couldn’t read “${file.path}” any more. Was it moved or changed?`);
	}

	/**
	 * Something failed while doing `step`: try again later if that may help,
	 * else fail. `again`: whether it may, when the status alone doesn't say.
	 */
	#trouble(u: Upload, step: Upload['step'], e: unknown, again = transient(statusOf(e))) {
		const status = statusOf(e);
		if (status === 404 && u.id)
			return this.#fail(
				u,
				'coversyncd no longer has this upload: it was cancelled elsewhere, the uploads folder changed, or it waited too long.'
			);
		if (!again) return this.#fail(u, messageOf(e));
		if (u.phase !== 'retrying') u.held = u.received;
		u.failures++;
		if (!unlimited(status) && u.failures > SERVER_FAILURES) return this.#fail(u, messageOf(e));
		this.#halt(u, 'retrying');
		u.step = step;
		u.error = messageOf(e);
		const delays = this.options.delays ?? DELAYS;
		const wait = delays[Math.min(u.failures, delays.length) - 1];
		u.retryAt = Date.now() + wait;
		u.timer = setTimeout(() => this.#again(u), wait);
		this.#changed(u);
	}

	/** Try again what failed. */
	async #again(u: Upload) {
		if (u.phase !== 'retrying' || u.asking) return;
		clearTimeout(u.timer);
		u.retryAt = null;
		this.#changed(u);
		if (u.step === 'start') return this.#start(u);
		if (u.step === 'finish') return this.#finish(u);
		u.asking = true;
		let server: ServerUpload;
		try {
			server = await this.transport.status(u.id!);
		} catch (e) {
			u.asking = false;
			if (u.phase === 'retrying') this.#trouble(u, 'resume', e);
			return;
		}
		u.asking = false;
		if (u.phase !== 'retrying') return;
		this.#plan(u, server);
		if (u.recount) {
			u.recount = false;
			u.held = 0;
		}
		u.phase = 'sending';
		u.error = null;
		this.#changed(u);
		this.#settle(u);
		this.#pump();
	}

	/** All sent: finish it. */
	#settle(u: Upload) {
		if (u.phase === 'sending' && !u.pending.length && !u.inflight.size) void this.#finish(u);
	}

	async #finish(u: Upload) {
		u.phase = 'finishing';
		u.error = null;
		this.#changed(u);
		try {
			u.result = await this.transport.finish(u.id!);
			if (u.phase !== 'finishing') return;
			u.phase = 'done';
			u.failures = 0;
		} catch (e) {
			if (u.phase !== 'finishing') return;
			// Bytes missing after all (lost in a reboot of the server, say): send
			// them, holding the progress shown until the server says what it has.
			if (statusOf(e) === 409) {
				u.recount = true;
				this.#trouble(u, 'resume', e, true);
			} else this.#trouble(u, 'finish', e);
		}
		this.#changed(u);
		// Uploads queued while it retried go on.
		this.#pump();
	}

	/** Fail, and remove what the server has of it: trying again starts over. */
	#fail(u: Upload, error: string) {
		const id = u.id;
		this.#halt(u, 'failed');
		u.error = error;
		u.id = null;
		if (id) this.#drop(id);
		this.#changed(u);
		this.#pump();
	}

	/** Move to `phase`, stopping the chunks in flight and any wait. */
	#halt(u: Upload, phase: Phase) {
		u.phase = phase;
		clearTimeout(u.timer);
		u.retryAt = null;
		u.pending = [];
		for (const f of u.inflight.values()) f.stop.abort();
		u.inflight.clear();
		if (phase !== 'retrying') u.held = 0;
	}

	/**
	 * Cancel upload `id` on the server, asking again while it can't be
	 * reached, for as long as the page is open (and once more as it goes).
	 */
	async #drop(id: string) {
		if (this.#deleting.has(id)) return;
		this.#deleting.add(id);
		const delays = this.options.delays ?? DELAYS;
		try {
			for (let i = 0; ; i++) {
				try {
					return await this.transport.cancel(id);
				} catch (e) {
					const status = statusOf(e);
					if (status === 404 || !transient(status)) return;
					if (!unlimited(status) && i >= SERVER_FAILURES) return;
					await new Promise((r) => setTimeout(r, delays[Math.min(i, delays.length - 1)]));
				}
			}
		} finally {
			this.#deleting.delete(id);
		}
	}
}
