import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Album, AlbumTags, TagWrite } from './api';

/** Each album's tags as the daemon has them, by album id. */
const stored = new Map<string, AlbumTags>();
/** Tag reads and saves asked for, in order: `read:<id>`, `save:<id>`. */
const calls: string[] = [];
/** Saves' bodies, by album id. */
const sent = new Map<string, TagWrite>();
/** How a save of an album answers instead of saving it. */
const refuse = new Map<string, 'conflict' | 'gone' | 'error' | 'warning' | 'blank' | 'lost'>();
/** Saves running at once, and the most there were. */
let running = 0;
let most = 0;

vi.mock('./api', () => {
	class ApiError extends Error {
		constructor(
			public status: number,
			message: string
		) {
			super(message);
		}
	}
	class TagConflict extends ApiError {
		constructor(
			message: string,
			public album: unknown
		) {
			super(409, message);
		}
	}
	return {
		ApiError,
		TagConflict,
		api: {
			album: (id: string) => {
				calls.push(`album:${id}`);
				const tags = stored.get(id);
				return tags
					? Promise.resolve({ id, title: `Title ${id}`, version: tags.version })
					: Promise.reject(new ApiError(404, 'Not found'));
			},
			albumTags: (id: string) => {
				calls.push(`read:${id}`);
				const tags = stored.get(id);
				return tags ? Promise.resolve(tags) : Promise.reject(new ApiError(404, 'Not found'));
			},
			writeTags: async (id: string, body: TagWrite) => {
				calls.push(`save:${id}`);
				sent.set(id, body);
				most = Math.max(most, ++running);
				await new Promise((r) => setTimeout(r, 1));
				running--;
				const how = refuse.get(id);
				if (how === 'conflict') throw new TagConflict('changed', { id: `renamed-${id}` });
				if (how === 'gone') throw new TagConflict('gone', null);
				if (how === 'error') throw new ApiError(400, 'The folder is already there: Anime/A');
				if (how === 'blank') throw new ApiError(502, '');
				if (how === 'lost') throw new TypeError('NetworkError when attempting to fetch resource.');
				return {
					album_id: `moved-${id}`,
					files_written: Object.keys(body.kinds ?? {}).length,
					files_moved: 2,
					warning: how === 'warning' ? "Saved, but the files weren't moved: busy" : undefined,
					album: null
				};
			}
		}
	};
});

const { albumValue, batchEdits, slim, tally, TagSaves } = await import('./tag-batch.svelte');

const album = (id: string) => ({ id, title: `Title ${id}` }) as Album;
/** Tags of an album whose tracks (ids from 1) have these values. */
function tagsOf(version: string, ...tracks: Record<string, string>[]): AlbumTags {
	return {
		version,
		tracks: tracks.map((values, i) => ({ id: i + 1, rel_path: `${i + 1}.flac`, values, other: [] }))
	};
}

beforeEach(() => {
	stored.clear();
	calls.length = 0;
	sent.clear();
	refuse.clear();
	running = most = 0;
});

describe('album values', () => {
	// Bugs caught: 0, False positives: 0
	it('is the value every track shares, empty for none, null when tracks differ', () => {
		const same = tagsOf('v', { genre: 'Anime' }, { genre: 'Anime' });
		const none = tagsOf('v', {}, { genre: '' });
		const mixed = tagsOf('v', { genre: 'Anime' }, {});
		expect(albumValue(same, 'genre')).toBe('Anime');
		expect(albumValue(none, 'genre')).toBe('');
		expect(albumValue(mixed, 'genre')).toBe(null);
	});

	// Bugs caught: 0, False positives: 0
	it('tallies albums per value, most first, albums differing within themselves last', () => {
		const albums = [
			tagsOf('v', { collection: 'Anime' }),
			tagsOf('v', {}),
			tagsOf('v', { collection: 'Anime' }),
			tagsOf('v', { collection: 'Anime' }, {}),
			tagsOf('v', { collection: 'Games' })
		];
		expect(tally(albums, 'collection')).toEqual([
			{ value: 'Anime', albums: 2 },
			{ value: '', albums: 1 },
			{ value: 'Games', albums: 1 },
			{ value: null, albums: 1 }
		]);
	});
});

describe('slim', () => {
	// Bugs caught: 0, False positives: 0
	it("keeps only what a batch needs of an album's tags: the version, track ids and paths, its three tags", () => {
		const tags = tagsOf('v', { genre: 'Pop', lyrics: 'la '.repeat(1000), album_artist: 'A' });
		tags.tracks[0].other = [['TITLE_JP', 'x']];
		expect(slim(tags)).toEqual({
			version: 'v',
			tracks: [
				{
					id: 1,
					rel_path: '1.flac',
					values: { collection: '', album_artist: 'A', genre: 'Pop' },
					other: []
				}
			]
		});
	});
});

describe('batch edits', () => {
	// Bugs caught: 0, False positives: 0
	it("sets each kind on the tracks that don't have it already", () => {
		const tags = tagsOf(
			'v',
			{ collection: 'Anime', genre: 'Pop' },
			{ genre: 'Pop' },
			{ collection: 'Anime', genre: 'Rock' }
		);
		expect(batchEdits(tags, { collection: 'Anime', genre: 'Pop' })).toEqual({
			2: { collection: 'Anime' },
			3: { genre: 'Pop' }
		});
	});

	// Bugs caught: 0, False positives: 0
	it("clears with '' (None), and has nothing for an album that already has the values", () => {
		const tags = tagsOf('v', { collection: 'Anime' }, {});
		expect(batchEdits(tags, { collection: '' })).toEqual({ 1: { collection: '' } });
		expect(batchEdits(tagsOf('v', { genre: 'Pop' }), { genre: 'Pop' })).toEqual({});
		expect(batchEdits(tags, {})).toEqual({});
	});
});

describe('saving', () => {
	// Bugs caught: 0, False positives: 0
	it('saves one album at a time, in order, on the versions read, skipping albums already set', async () => {
		stored.set('a', tagsOf('va', {}, {}));
		stored.set('b', tagsOf('vb', { collection: 'Anime' }));
		stored.set('c', tagsOf('vc', { collection: 'Games' }));
		const finished = vi.fn();
		const saves = new TagSaves({ finished });
		const set = { collection: 'Anime' };
		await saves.add(['a', 'b', 'c'].map((id) => ({ album: album(id), set })));
		expect(calls).toEqual(['read:a', 'save:a', 'read:b', 'read:c', 'save:c']);
		expect(most).toBe(1);
		expect(sent.get('a')).toEqual({
			kinds: { 1: { collection: 'Anime' }, 2: { collection: 'Anime' } },
			version: 'va',
			base_tracks: [
				{ id: 1, rel_path: '1.flac' },
				{ id: 2, rel_path: '2.flac' }
			]
		});
		expect(finished).toHaveBeenCalledOnce();
		const items = finished.mock.calls[0][0];
		expect(items.map((i: { status: string }) => i.status)).toEqual(['saved', 'unchanged', 'saved']);
		expect(items[0].moved).toBe(2);
		expect(saves.running).toBe(false);
		expect(saves.items).toEqual([]);
	});

	// Bugs caught: 0, False positives: 0
	it('uses tags read beforehand, so an album changed since is refused, not overwritten', async () => {
		stored.set('a', tagsOf('new', { genre: 'Rock' }));
		refuse.set('a', 'conflict');
		const finished = vi.fn();
		const saves = new TagSaves({ finished });
		await saves.add([{ album: album('a'), set: { genre: 'Pop' }, tags: tagsOf('old', {}) }]);
		expect(calls).toEqual(['save:a']);
		expect(sent.get('a')?.version).toBe('old');
		const [item] = finished.mock.calls[0][0];
		expect(item.status).toBe('failed');
		expect(item.problem).toMatch(/changed elsewhere/i);
		// The album as it is now, under its new id: Select these finds it.
		expect(item.album.id).toBe('renamed-a');
	});

	// Bugs caught: 0, False positives: 0
	it('refuses an album whose tags read beforehand had the values, if it changed since', async () => {
		stored.set('a', tagsOf('new', { genre: 'Jazz' }));
		stored.set('b', tagsOf('same', { genre: 'Pop' }));
		const finished = vi.fn();
		const saves = new TagSaves({ finished });
		await saves.add([
			{ album: album('a'), set: { genre: 'Pop' }, tags: tagsOf('old', { genre: 'Pop' }) },
			{ album: album('b'), set: { genre: 'Pop' }, tags: tagsOf('same', { genre: 'Pop' }) }
		]);
		expect(calls).toEqual(['album:a', 'album:b']);
		const [a, b] = finished.mock.calls[0][0];
		expect(a.status).toBe('failed');
		expect(a.problem).toMatch(/changed elsewhere/i);
		expect(b.status).toBe('unchanged');
	});

	// Bugs caught: 0, False positives: 0
	it('goes on past an album that fails, and says why for each; one saved but not moved is a problem too', async () => {
		for (const id of ['a', 'b', 'c', 'd', 'e', 'f']) stored.set(id, tagsOf(id, {}));
		refuse.set('a', 'error');
		refuse.set('b', 'gone');
		refuse.set('c', 'warning');
		refuse.set('e', 'blank');
		refuse.set('f', 'lost');
		const finished = vi.fn();
		const saves = new TagSaves({ finished });
		await saves.add(
			['a', 'b', 'c', 'd', 'missing', 'e', 'f'].map((id) => ({
				album: album(id),
				set: { genre: 'Pop' }
			}))
		);
		const items = finished.mock.calls[0][0] as { status: string; problem: string | null }[];
		expect(items.map((i) => i.status)).toEqual([
			'failed',
			'failed',
			'saved',
			'saved',
			'failed',
			'failed',
			'failed'
		]);
		expect(items[0].problem).toBe('The folder is already there: Anime/A');
		expect(items[1].problem).toMatch(/no longer in the Library/);
		expect(items[2].problem).toMatch(/weren't moved/);
		expect(items[3].problem).toBe(null);
		expect(items[4].problem).toMatch(/no longer in the Library/);
		// An error with no message still says something, so it's listed.
		expect(items[5].problem).toMatch(/HTTP 502/);
		// No answer: the daemon may have saved it all the same.
		expect(items[6].problem).toMatch(/couldn't confirm/i);
	});

	// Bugs caught: 0, False positives: 0
	it('runs a batch added while one is saving after it, as one run', async () => {
		for (const id of ['a', 'b', 'c']) stored.set(id, tagsOf(id, {}));
		const finished = vi.fn();
		const progress = vi.fn();
		const saves = new TagSaves({ finished, progress });
		const first = saves.add([{ album: album('a'), set: { genre: 'Pop' } }]);
		expect(saves.running).toBe(true);
		expect(saves.status('a')).not.toBe(null);
		const second = saves.add(
			['b', 'c'].map((id) => ({ album: album(id), set: { genre: 'Rock' } }))
		);
		expect(saves.status('c')).toBe('waiting');
		await Promise.all([first, second]);
		expect(calls).toEqual(['read:a', 'save:a', 'read:b', 'save:b', 'read:c', 'save:c']);
		expect(most).toBe(1);
		expect(finished).toHaveBeenCalledOnce();
		expect(finished.mock.calls[0][0]).toHaveLength(3);
		expect(progress).toHaveBeenCalled();
		expect(saves.status('c')).toBe(null);
	});
});
