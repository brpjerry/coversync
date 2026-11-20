import { describe, expect, it, vi } from 'vitest';
import type { Album } from './api';

/** Lookups asked for, in order, and how many status polls each takes. */
const asked: string[] = [];
let polls = 0;
const imports: { id: string; fail?: string }[] = [];

vi.mock('./api', () => {
	class ApiError extends Error {}
	return {
		ApiError,
		api: {
			importPlan: (id: string) => Promise.resolve({ id, failing: [] }),
			identify: (id: string) => {
				asked.push(id);
				polls = 2;
				return Promise.resolve({ state: 'running' });
			},
			identifyStatus: () => Promise.resolve({ state: --polls > 0 ? 'running' : 'done' }),
			importAlbum: (id: string) => {
				const fail = imports.find((i) => i.id === id)?.fail;
				return fail
					? Promise.reject(new ApiError(fail))
					: Promise.resolve({ album_id: `lib-${id}`, dest_dir: `/lib/${id}`, files: 1 });
			}
		}
	};
});

const { ImportBatch } = await import('./batch.svelte');

const albums = (...ids: string[]) => ids.map((id) => ({ id, title: id }) as Album);
const noWait = () => Promise.resolve();

describe('a batch', () => {
	it('goes to the next album waiting, coming round to skipped-over ones, then ends', () => {
		const b = new ImportBatch(albums('a', 'b', 'c', 'd'), true);
		b.skip();
		b.advance();
		expect(b.current.album.id).toBe('b');
		// Jumping ahead leaves b waiting: it comes round again after d.
		b.index = 2;
		b.items[2].status = 'imported';
		b.advance();
		expect(b.current.album.id).toBe('d');
		expect(b.last).toBe(false);
		b.items[3].status = 'importing';
		b.advance();
		expect(b.current.album.id).toBe('b');
		expect(b.last).toBe(true);
		b.skip();
		b.advance();
		expect(b.ended).toBe(true);
		expect([b.count('skipped'), b.count('imported'), b.count('importing')]).toEqual([2, 1, 1]);
	});

	it('fetches lookups one at a time, the current album first, then the next ones in order', async () => {
		asked.length = 0;
		const b = new ImportBatch(albums('a', 'b', 'c', 'd'), true);
		b.index = 2;
		b.items[3].status = 'skipped';
		await b.prefetch(noWait);
		expect(asked).toEqual(['c', 'a', 'b']);
		expect(b.items.map((i) => i.lookup)).toEqual(['done', 'done', 'done', 'idle']);
		// With Smart suggestions off there's nothing to look up.
		asked.length = 0;
		await new ImportBatch(albums('x'), false).prefetch(noWait);
		expect(asked).toEqual([]);
	});

	it('stops fetching ahead once it ends', async () => {
		asked.length = 0;
		const b = new ImportBatch(albums('a', 'b', 'c'), true);
		const fetching = b.prefetch(noWait);
		b.ended = true;
		await fetching;
		expect(asked).toEqual(['a']);
	});

	it('records where an import went, or why it failed', async () => {
		imports.push({ id: 'b', fail: 'The folder is already there' });
		const b = new ImportBatch(albums('a', 'b'), true);
		const [a, bad] = b.items;
		await Promise.all([b.import(a, {}), b.import(bad, {})]);
		expect([a.status, a.dest, a.libraryId]).toEqual(['imported', '/lib/a', 'lib-a']);
		expect([bad.status, bad.error]).toEqual(['failed', 'The folder is already there']);
		// A failed album can be skipped, and isn't waiting any more.
		b.index = 1;
		b.skip();
		expect(b.waiting).toBe(0);
	});

	it('plans every album', async () => {
		const b = new ImportBatch(albums('a', 'b', 'c', 'd', 'e'), true);
		await b.loadPlans(2);
		expect(b.items.every((i) => i.plan)).toBe(true);
	});
});
