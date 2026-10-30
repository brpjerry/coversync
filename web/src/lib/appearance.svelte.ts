// Appearance: the top bar's sort tabs, the sidebar's tag filters and the album
// grid's cover size. Loaded at start-up because the top bar, sidebar and grid
// need them; the Appearance screen edits `sorts`, `filters` (the top bar
// previews the sort tabs) and `coverSize`.

import { api, ApiError, type AppearanceResponse, type CoverSizes, type Sortable } from './api';
import { toast } from './toast.svelte';

/** Match the daemon's defaults, so the top bar and sidebar are right before it answers. */
const DEFAULT_SORTS = ['album_artist', 'album', 'date', 'added', 'attention'];
const DEFAULT_FILTERS = ['genre', 'album_artist', 'date'];
const DEFAULT_LABELS: Record<string, string> = {
	album_artist: 'Album artist',
	album: 'Album',
	date: 'Year',
	genre: 'Genre',
	added: 'Recently added',
	attention: 'Needs attention'
};
const COVER_SIZES: CoverSizes = { min: 120, max: 320, default: 176 };

/** An ordered list of tags, edited apart from what was last saved. */
export class TagList {
	/** How many the list must keep. */
	min: number;
	defaults = $state<string[]>([]);
	/** As last saved on the daemon. */
	saved = $state<string[]>([]);
	/** Being edited. */
	items = $state<string[]>([]);

	constructor(defaults: string[], min: number) {
		this.min = min;
		this.set(defaults, defaults);
	}

	changes = $derived.by(() => {
		const added = this.items.filter((t) => !this.saved.includes(t)).length;
		const removed = this.saved.filter((t) => !this.items.includes(t)).length;
		const kept = this.items.filter((t) => this.saved.includes(t));
		const reordered = kept.some((t, i) => t !== this.saved.filter((s) => kept.includes(s))[i]);
		return added + removed + (reordered ? 1 : 0);
	});
	isDefault = $derived(this.items.join() === this.defaults.join());

	set(saved: string[], defaults: string[]) {
		this.defaults = defaults;
		this.saved = saved;
		this.items = [...saved];
	}

	/** Whether `id` can come off the list. */
	removable(id: string) {
		return !this.items.includes(id) || this.items.length > this.min;
	}

	toggle(id: string) {
		if (this.items.includes(id)) this.remove(id);
		else this.items = [...this.items, id];
	}

	remove(id: string) {
		if (this.removable(id)) this.items = this.items.filter((t) => t !== id);
	}

	move(id: string, by: number) {
		const i = this.items.indexOf(id);
		const j = i + by;
		if (i < 0 || j < 0 || j >= this.items.length) return;
		const next = [...this.items];
		[next[i], next[j]] = [next[j], next[i]];
		this.items = next;
	}

	reset() {
		this.items = [...this.defaults];
	}

	revert() {
		this.items = [...this.saved];
	}
}

class AppearanceStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
	sortable = $state<Sortable[]>([]);
	filterable = $state<Sortable[]>([]);
	groups = $state<{ id: string; label: string }[]>([]);
	/** The top bar's sort tabs; it keeps at least one. */
	sorts = new TagList(DEFAULT_SORTS, 1);
	/** Tags the sidebar filters by. */
	filters = new TagList(DEFAULT_FILTERS, 0);
	coverSizes = $state<CoverSizes>(COVER_SIZES);
	/** Album grid cover size as last saved, in px. */
	savedCoverSize = $state(COVER_SIZES.default);
	/** Cover size being edited. */
	coverSize = $state(COVER_SIZES.default);

	changes = $derived(
		this.sorts.changes + this.filters.changes + (this.coverSize !== this.savedCoverSize ? 1 : 0)
	);
	/** Loaded, or given up on: the grid can lay out without jumping later. */
	settled = $derived(this.loaded || this.failed);

	#apply(r: AppearanceResponse) {
		this.sortable = r.sortable;
		this.filterable = r.filterable;
		this.groups = r.groups;
		this.sorts.set(r.setup.sort_tabs, r.defaults);
		this.filters.set(r.setup.filter_tags, r.filter_defaults);
		this.coverSizes = r.cover_sizes;
		this.savedCoverSize = this.coverSize = r.setup.cover_size;
	}

	async load() {
		try {
			this.#apply(await api.appearance());
			this.loaded = true;
		} catch {
			this.failed = !this.loaded;
		}
	}

	label(id: string) {
		return this.sortable.find((s) => s.id === id)?.label ?? DEFAULT_LABELS[id] ?? id;
	}

	revert() {
		this.sorts.revert();
		this.filters.revert();
		this.coverSize = this.savedCoverSize;
	}

	/** Returns whether it saved; new tabs and filters need fresh album data for their values. */
	async save(): Promise<boolean> {
		this.saving = true;
		try {
			this.#apply(
				await api.saveAppearance({
					sort_tabs: this.sorts.items,
					filter_tags: this.filters.items,
					cover_size: this.coverSize
				})
			);
			toast('Appearance saved', 'success');
			return true;
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
			return false;
		} finally {
			this.saving = false;
		}
	}
}

export const appearance = new AppearanceStore();
