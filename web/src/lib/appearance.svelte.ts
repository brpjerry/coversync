// Appearance: the sidebar's sort tabs and the album grid's cover size. Loaded
// at start-up because the sidebar and grid need them; the Appearance screen
// edits `tabs` (the sidebar previews them) and `coverSize`.

import { api, ApiError, type AppearanceResponse, type CoverSizes, type Sortable } from './api';
import { toast } from './toast.svelte';

/** Matches the daemon's defaults, so the sidebar is right before it answers. */
const DEFAULTS = ['album_artist', 'album', 'date', 'added', 'attention'];
const DEFAULT_LABELS: Record<string, string> = {
	album_artist: 'Album artist',
	album: 'Album',
	date: 'Year',
	added: 'Recently added',
	attention: 'Needs attention'
};
const COVER_SIZES: CoverSizes = { min: 120, max: 320, default: 176 };

class AppearanceStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
	sortable = $state<Sortable[]>([]);
	groups = $state<{ id: string; label: string }[]>([]);
	defaults = $state<string[]>(DEFAULTS);
	/** Sort tabs as last saved on the daemon. */
	saved = $state<string[]>(DEFAULTS);
	/** Sort tabs being edited. */
	tabs = $state<string[]>(DEFAULTS);
	coverSizes = $state<CoverSizes>(COVER_SIZES);
	/** Album grid cover size as last saved, in px. */
	savedCoverSize = $state(COVER_SIZES.default);
	/** Cover size being edited. */
	coverSize = $state(COVER_SIZES.default);

	changes = $derived.by(() => {
		const added = this.tabs.filter((t) => !this.saved.includes(t)).length;
		const removed = this.saved.filter((t) => !this.tabs.includes(t)).length;
		const kept = this.tabs.filter((t) => this.saved.includes(t));
		const reordered = kept.some((t, i) => t !== this.saved.filter((s) => kept.includes(s))[i]);
		const resized = this.coverSize !== this.savedCoverSize;
		return added + removed + (reordered ? 1 : 0) + (resized ? 1 : 0);
	});
	isDefault = $derived(this.tabs.join() === this.defaults.join());
	/** Loaded, or given up on: the grid can lay out without jumping later. */
	settled = $derived(this.loaded || this.failed);

	#apply(r: AppearanceResponse) {
		this.sortable = r.sortable;
		this.groups = r.groups;
		this.defaults = r.defaults;
		this.saved = r.setup.sort_tabs;
		this.tabs = [...r.setup.sort_tabs];
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

	toggle(id: string) {
		if (this.tabs.includes(id)) this.remove(id);
		else this.tabs = [...this.tabs, id];
	}

	remove(id: string) {
		if (this.tabs.length > 1) this.tabs = this.tabs.filter((t) => t !== id);
	}

	move(id: string, by: number) {
		const i = this.tabs.indexOf(id);
		const j = i + by;
		if (i < 0 || j < 0 || j >= this.tabs.length) return;
		const next = [...this.tabs];
		[next[i], next[j]] = [next[j], next[i]];
		this.tabs = next;
	}

	reset() {
		this.tabs = [...this.defaults];
	}

	revert() {
		this.tabs = [...this.saved];
		this.coverSize = this.savedCoverSize;
	}

	/** Returns whether it saved; new tabs need fresh album data for their values. */
	async save(): Promise<boolean> {
		this.saving = true;
		try {
			this.#apply(await api.saveAppearance({ sort_tabs: this.tabs, cover_size: this.coverSize }));
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
