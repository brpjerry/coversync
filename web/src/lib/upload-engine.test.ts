import { describe, expect, it } from 'vitest';
import type { FinishedUpload, ServerUpload } from './api';
import { Uploader, type Transport, type Upload } from './upload-engine';
import type { Picked } from './upload-files';

const KiB = 1024;
const MiB = 1024 * KiB;

/** Bytes that differ at every offset, so a chunk written in the wrong place shows. */
function bytes(n: number, seed: number) {
	const out = new Uint8Array(n);
	for (let i = 0; i < n; i++) out[i] = (i * 31 + seed + (i >> 10)) & 0xff;
	return out;
}

function album(...files: [string, number][]): Picked {
	return {
		folder: 'Album',
		files: files.map(([path, size], i) => ({ path, file: new Blob([bytes(size, i)]) })),
		skipped: []
	};
}

class HttpError extends Error {
	constructor(
		public status: number,
		message = `HTTP ${status}`
	) {
		super(message);
	}
}

interface Held {
	id: string;
	folder: string;
	files: { path: string; size: number; data: Uint8Array; ranges: [number, number][] }[];
}

/**
 * The upload API in memory, as SRV-05 behaves: chunks at any offset, the
 * bytes of a cut chunk kept, finishing refused while bytes are missing.
 */
class FakeServer implements Transport {
	uploads = new Map<string, Held>();
	finished: { folder: string; files: Map<string, Uint8Array> }[] = [];
	cancelled: string[] = [];
	puts: { index: number; offset: number; length: number }[] = [];
	inflight = 0;
	maxInflight = 0;
	bytesInflight = 0;
	maxBytesInflight = 0;
	/** Cuts the next chunk sent to `index` at `offset`, after `after` bytes. */
	cut: { index: number; offset: number; after: number } | null = null;
	/** Answers a chunk with this status instead. */
	refuse: { index: number; status: number } | null = null;
	/** While set, starts wait for it. */
	holdStarts: Promise<void> | null = null;
	/** While set, chunks wait for it. */
	holdChunks: Promise<void> | null = null;
	#next = 0;

	async start(folder: string, files: { path: string; size: number }[]): Promise<ServerUpload> {
		await this.holdStarts;
		const id = `u${this.#next++}`;
		this.uploads.set(id, {
			id,
			folder,
			files: files.map((f) => ({ ...f, data: new Uint8Array(f.size), ranges: [] }))
		});
		return this.status(id);
	}

	async status(id: string): Promise<ServerUpload> {
		const u = this.uploads.get(id);
		if (!u) throw new HttpError(404);
		const files = u.files.map((f, index) => ({
			index,
			path: f.path,
			size: f.size,
			ranges: f.ranges.map(([a, b]) => [a, b] as [number, number])
		}));
		const size = u.files.reduce((n, f) => n + f.size, 0);
		const received = u.files.reduce((n, f) => n + f.ranges.reduce((m, [a, b]) => m + b - a, 0), 0);
		return { id, folder: u.folder, size, received, files };
	}

	async put(
		id: string,
		index: number,
		offset: number,
		body: Blob,
		progress: (sent: number) => void,
		signal: AbortSignal
	) {
		this.inflight++;
		this.maxInflight = Math.max(this.maxInflight, this.inflight);
		this.bytesInflight += body.size;
		this.maxBytesInflight = Math.max(this.maxBytesInflight, this.bytesInflight);
		this.puts.push({ index, offset, length: body.size });
		try {
			const aborted = new Promise<never>((_, fail) =>
				signal.addEventListener('abort', () => fail(new HttpError(0, 'aborted')))
			);
			await Promise.race([this.holdChunks, aborted]);
			const u = this.uploads.get(id);
			if (!u) throw new HttpError(404);
			if (this.refuse?.index === index) throw new HttpError(this.refuse.status, 'refused');
			// A file the browser can't read fails the request, as a dropped connection does.
			const read = await body.arrayBuffer().catch(() => null);
			if (!read) throw new HttpError(0);
			const data = new Uint8Array(read);
			const f = u.files[index];
			const cut = this.cut?.index === index && this.cut.offset === offset ? this.cut : null;
			const keep = cut ? cut.after : data.length;
			// Half way, then the rest: progress as a browser reports it.
			const half = Math.floor(keep / 2);
			for (const [from, to] of [
				[0, half],
				[half, keep]
			]) {
				await Promise.resolve();
				if (signal.aborted) throw new HttpError(0, 'aborted');
				f.data.set(data.subarray(from, to), offset + from);
				f.ranges = merge(f.ranges, [offset + from, offset + to]);
				progress(to);
			}
			if (cut) {
				this.cut = null;
				throw new HttpError(0, 'The connection to coversyncd was lost.');
			}
		} finally {
			this.inflight--;
			this.bytesInflight -= body.size;
		}
	}

	async finish(id: string): Promise<FinishedUpload> {
		const u = this.uploads.get(id);
		if (!u) throw new HttpError(404);
		const missing = u.files.find(
			(f) => f.size && !(f.ranges.length === 1 && f.ranges[0][1] === f.size)
		);
		if (missing) throw new HttpError(409, `“${missing.path}” hasn't arrived in full yet.`);
		this.uploads.delete(id);
		this.finished.push({ folder: u.folder, files: new Map(u.files.map((f) => [f.path, f.data])) });
		return { folder: u.folder, path: `/uploads/${u.folder}` };
	}

	async cancel(id: string) {
		if (!this.uploads.delete(id)) throw new HttpError(404);
		this.cancelled.push(id);
	}
}

function merge(ranges: [number, number][], [start, end]: [number, number]) {
	if (start === end) return ranges;
	const all = [...ranges, [start, end] as [number, number]].sort((a, b) => a[0] - b[0]);
	const out: [number, number][] = [];
	for (const [a, b] of all) {
		const last = out[out.length - 1];
		if (last && a <= last[1]) last[1] = Math.max(last[1], b);
		else out.push([a, b]);
	}
	return out;
}

/** An uploader on `server`, recording the progress it reports. */
function uploader(server: FakeServer, parallel = 4, budget?: number) {
	const progress: number[] = [];
	const engine = new Uploader(server, {
		parallel,
		budget,
		chunk: MiB,
		delays: [1],
		changed: (u) => progress.push(u.received)
	});
	const until = (u: Upload, phase: Upload['phase']) =>
		new Promise<void>((resolve) => {
			const check = () => (u.phase === phase ? resolve() : setTimeout(check, 1));
			check();
		});
	return { engine, progress, until };
}

async function sameBytes(server: FakeServer, picked: Picked) {
	const [done] = server.finished;
	for (const f of picked.files)
		expect(done.files.get(f.path)).toEqual(new Uint8Array(await f.file.arrayBuffer()));
}

describe('Uploader', () => {
	it('sends large files in chunks, several at once, with progress that only goes up', async () => {
		const server = new FakeServer();
		// Up to 16 requests, but no more than 4 chunks' worth of bytes.
		const { engine, progress, until } = uploader(server, 16, 4 * MiB);
		const picked = album(
			['CD1/01.flac', 10 * MiB + 123],
			['CD1/02.flac', 300 * KiB],
			['CD2/01.flac', 3 * MiB]
		);
		const upload = engine.add(picked);
		await until(upload, 'done');

		await sameBytes(server, picked);
		expect(upload.result).toEqual({ folder: 'Album', path: '/uploads/Album' });
		expect(server.maxInflight).toBeGreaterThanOrEqual(4);
		expect(server.maxBytesInflight).toBeLessThanOrEqual(4 * MiB);
		expect(Math.max(...server.puts.map((p) => p.length))).toBe(MiB);
		expect(progress.at(-1)).toBe(upload.size);
		expect(progress).toEqual([...progress].sort((a, b) => a - b));

		// Small files: as many requests at once as allowed.
		const small = new FakeServer();
		const many = uploader(small, 16, 4 * MiB);
		const files = Array.from({ length: 40 }, (_, i): [string, number] => [`${i}.flac`, 64 * KiB]);
		const second = many.engine.add(album(...files));
		await many.until(second, 'done');
		expect(small.maxInflight).toBe(16);
	});

	it('carries on from what the server kept after the connection drops mid-chunk', async () => {
		const server = new FakeServer();
		server.cut = { index: 0, offset: 2 * MiB, after: 300 * KiB };
		const { engine, progress, until } = uploader(server);
		const picked = album(['01.flac', 5 * MiB], ['02.flac', 2 * MiB]);
		const upload = engine.add(picked);
		await until(upload, 'done');

		await sameBytes(server, picked);
		// The rest of the cut chunk went from where the server's copy ended.
		expect(server.puts).toContainEqual({
			index: 0,
			offset: 2 * MiB + 300 * KiB,
			length: MiB - 300 * KiB
		});
		// Shown progress held while it reconnected.
		expect(progress).toEqual([...progress].sort((a, b) => a - b));
	});

	it('cancelling stops the chunks in flight and leaves nothing on the server', async () => {
		const server = new FakeServer();
		// Chunks that never get through, until they're stopped.
		server.holdChunks = new Promise(() => {});
		const { engine, until } = uploader(server);
		const sending = engine.add(album(['01.flac', 8 * MiB]));
		await until(sending, 'sending');
		expect(server.inflight).toBe(4);
		engine.cancel(sending);
		await until(sending, 'cancelled');
		await new Promise((r) => setTimeout(r, 5));
		expect(server.inflight).toBe(0);
		expect(server.uploads.size).toBe(0);

		// Cancelled while the server was starting it: removed once it answers.
		let open = () => {};
		server.holdStarts = new Promise((r) => (open = r));
		const starting = engine.add(album(['01.flac', MiB]));
		await until(starting, 'starting');
		engine.cancel(starting);
		open();
		await new Promise((r) => setTimeout(r, 5));
		expect(server.uploads.size).toBe(0);
		expect(server.cancelled).toHaveLength(2);
		expect(server.finished).toHaveLength(0);
	});

	it('a failed upload leaves nothing on the server, and trying again starts over', async () => {
		const server = new FakeServer();
		server.refuse = { index: 1, status: 422 };
		const { engine, until } = uploader(server);
		const picked = album(['01.flac', 3 * MiB], ['02.flac', MiB]);
		const upload = engine.add(picked);
		await until(upload, 'failed');
		expect(upload.error).toBe('refused');
		await new Promise((r) => setTimeout(r, 5));
		expect(server.uploads.size).toBe(0);

		server.refuse = null;
		engine.retry(upload);
		await until(upload, 'done');
		await sameBytes(server, picked);

		// A file the browser can't read any more fails it too, rather than retrying for ever.
		const gone: Picked = {
			folder: 'Gone',
			files: [{ path: '01.flac', file: unreadable(2 * MiB) }],
			skipped: []
		};
		const failing = engine.add(gone);
		await until(failing, 'failed');
		expect(failing.error).toContain('01.flac');
		await new Promise((r) => setTimeout(r, 5));
		expect(server.uploads.size).toBe(0);
	});
});

/** A file that was moved after it was picked: its size is known, but reading it fails. */
function unreadable(size: number): Blob {
	const fail = () => Promise.reject(new DOMException('gone', 'NotReadableError'));
	const part = { size: 1, arrayBuffer: fail };
	return { size, slice: () => part } as unknown as Blob;
}
