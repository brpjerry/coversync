import { describe, expect, it, vi } from 'vitest';
import type { Album } from './api';

/** Lookups and plans asked for, in order. */
const asked: string[] = [];
const planned: string[] = [];
let polls = 0;
const imports: { id: string; fail?: string }[] = [];
/** Albums whose plan says Smart suggestions don't apply (hybrid WavPack, say). */
const noSuggest = new Set<string>();

vi.mock('./api', () => {
	class ApiError extends Error {}
	return {
		ApiError,
		api: {
			importPlan: (id: string) => {
				planned.push(id);
				return Promise.resolve({ id, failing: [], suggest: !noSuggest.has(id) });
			},
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
	it('goes to the next album waiting, coming round to skipped-over ones', () => {
		const b = new ImportBatch(albums('a', 'b', 'c', 'd'), true);
		b.skip();
		expect(b.nextWaiting()).toBe(1);
		// Jumping ahead leaves b waiting: it comes round again after d.
		b.index = 2;
		b.items[2].status = 'imported';
		expect(b.nextWaiting()).toBe(3);
		expect(b.last).toBe(false);
		b.index = 3;
		b.items[3].status = 'importing';
		expect(b.nextWaiting()).toBe(1);
		b.index = 1;
		expect(b.last).toBe(true);
		b.skip();
		expect(b.nextWaiting()).toBe(null);
		expect([b.count('skipped'), b.count('imported'), b.count('importing')]).toEqual([2, 1, 1]);
	});

	it('asks for an album’s first plan once, and keeps a newer one', async () => {
		planned.length = 0;
		const b = new ImportBatch(albums('a'), true);
		const [a] = b.items;
		const [one, two] = [b.firstPlan(a), b.firstPlan(a)];
		await Promise.all([one, two]);
		expect(planned).toEqual(['a']);
		// A plan with edits, made while another first plan was on its way, stays.
		a.plan = null;
		const first = b.firstPlan(a);
		const edited = { id: 'a', failing: ['date'] } as never;
		a.plan = edited;
		await first;
		expect(a.plan).toBe(edited);
	});

	it('looks albums up one at a time, the current first, once their plans say they use it', async () => {
		asked.length = 0;
		noSuggest.add('d');
		const b = new ImportBatch(albums('a', 'b', 'c', 'd', 'e'), true);
		b.index = 2;
		b.items[4].status = 'skipped';
		await Promise.all(b.items.map((i) => b.firstPlan(i)));
		await b.prefetch(noWait);
		expect(asked).toEqual(['c', 'a', 'b']);
		expect(b.items.map((i) => i.lookup)).toEqual(['done', 'done', 'done', 'idle', 'idle']);
		noSuggest.clear();
		// With Smart suggestions off there's nothing to look up.
		asked.length = 0;
		const off = new ImportBatch(albums('x'), false);
		await off.firstPlan(off.items[0]);
		await off.prefetch(noWait);
		expect(asked).toEqual([]);
	});

	it('stops planning and looking up once it ends', async () => {
		asked.length = 0;
		planned.length = 0;
		const b = new ImportBatch(albums('a', 'b', 'c'), true);
		await b.firstPlan(b.items[0]);
		const fetching = b.prefetch(noWait);
		b.ended = true;
		await fetching;
		await b.loadPlans();
		expect(asked).toEqual(['a']);
		expect(planned).toEqual(['a']);
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
});
