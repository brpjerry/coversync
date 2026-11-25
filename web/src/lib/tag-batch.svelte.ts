// Editing several Library albums' tags at once (Edit tags… and Collection ▾
// in the Library's selection bar): a few album-wide tags set on every track
// of each album, saved one album after another as the album editor saves.

import { api, ApiError, TagConflict, type Album, type AlbumTags } from './api';

/** The tags the batch sets: Collection, Album artist and Genre. */
export const BATCH_KINDS = ['collection', 'album_artist', 'genre'] as const;
export type BatchKind = (typeof BATCH_KINDS)[number];
/** The value to give each kind set; a kind left out keeps each album's own. '' clears (None). */
export type BatchSet = Partial<Record<BatchKind, string>>;

/** The value every track of the album has for `kind` ('' for none), or null when they differ. */
export function albumValue(tags: AlbumTags, kind: string): string | null {
	const seen = new Set(tags.tracks.map((t) => t.values[kind] ?? ''));
	return seen.size <= 1 ? ([...seen][0] ?? '') : null;
}

/**
 * How many albums have each value of `kind`, most first. `value` null counts
 * albums whose tracks differ, listed last.
 */
export function tally(all: AlbumTags[], kind: string): { value: string | null; albums: number }[] {
	const counts = new Map<string | null, number>();
	for (const tags of all) {
		const value = albumValue(tags, kind);
		counts.set(value, (counts.get(value) ?? 0) + 1);
	}
	return [...counts]
		.map(([value, albums]) => ({ value, albums }))
		.sort((a, b) => +(a.value === null) - +(b.value === null) || b.albums - a.albums);
}

/** What saving `set` writes to the album: by track id, each kind the track doesn't have that value of yet. */
export function batchEdits(tags: AlbumTags, set: BatchSet): Record<number, Record<string, string>> {
	const out: Record<number, Record<string, string>> = {};
	for (const t of tags.tracks)
		for (const [kind, value] of Object.entries(set))
			if ((t.values[kind] ?? '') !== value) (out[t.id] ??= {})[kind] = value;
	return out;
}

export type SaveStatus = 'waiting' | 'saving' | 'saved' | 'unchanged' | 'failed';

/** An album to save. */
export interface SaveJob {
	album: Album;
	set: BatchSet;
	/** Its tags as shown when the values were picked: a save refuses an album changed since. Read just before its save if none. */
	tags?: AlbumTags | null;
}

export interface SaveItem {
	album: Album;
	set: BatchSet;
	tags: AlbumTags | null;
	status: SaveStatus;
	/** Why it failed, or what went wrong with a save (files not moved). */
	problem: string | null;
	/** Files the save moved to fit the folder structure. */
	moved: number;
}

/**
 * Albums saved one after another. A batch added while others save joins
 * their run; `finished` hears the run's albums once all are done.
 */
export class TagSaves {
	items = $state<SaveItem[]>([]);
	#run: Promise<void> | null = null;

	constructor(
		private hooks: {
			/** An album started or finished saving. */
			progress?: (saves: TagSaves) => void;
			finished?: (items: SaveItem[]) => void;
		} = {}
	) {}

	/** Albums are waiting or saving. */
	get running(): boolean {
		return this.items.some((i) => i.status === 'waiting' || i.status === 'saving');
	}

	/** Whether the album with this id is waiting its turn or being saved. */
	status(id: string): 'waiting' | 'saving' | null {
		const item = this.items.find(
			(i) => i.album.id === id && (i.status === 'waiting' || i.status === 'saving')
		);
		return (item?.status as 'waiting' | 'saving' | undefined) ?? null;
	}

	count(status: SaveStatus): number {
		return this.items.filter((i) => i.status === status).length;
	}

	/** Save these albums after any already waiting. Resolves when the run is done. */
	add(jobs: SaveJob[]): Promise<void> {
		for (const { album, set, tags } of jobs)
			this.items.push({
				album,
				set,
				tags: tags ?? null,
				status: 'waiting',
				problem: null,
				moved: 0
			});
		this.hooks.progress?.(this);
		return (this.#run ??= this.#saveAll().finally(() => (this.#run = null)));
	}

	async #saveAll() {
		let item: SaveItem | undefined;
		while ((item = this.items.find((i) => i.status === 'waiting'))) {
			item.status = 'saving';
			this.hooks.progress?.(this);
			await save(item);
			this.hooks.progress?.(this);
		}
		const done = this.items;
		this.items = [];
		this.hooks.finished?.(done);
	}
}

const GONE = "It's no longer in the Library under this name: it may have moved or been removed.";

async function save(item: SaveItem) {
	try {
		const tags = item.tags ?? (await api.albumTags(item.album.id));
		const kinds = batchEdits(tags, item.set);
		if (!Object.keys(kinds).length) {
			item.status = 'unchanged';
			return;
		}
		const result = await api.writeTags(item.album.id, {
			kinds,
			version: tags.version,
			base_tracks: tags.tracks.map(({ id, rel_path }) => ({ id, rel_path }))
		});
		item.status = 'saved';
		item.moved = result.files_moved;
		item.problem = result.warning ?? null;
	} catch (e) {
		item.status = 'failed';
		if (e instanceof TagConflict)
			item.problem = e.album
				? "It changed elsewhere after its tags were read, so it wasn't saved."
				: GONE;
		else if (e instanceof ApiError) item.problem = e.status === 404 ? GONE : e.message;
		else item.problem = "It wasn't saved: the server couldn't be reached.";
	}
}
