// Library setup: where managed files live, how they are laid out, and the
// rules they follow. Kept at module level so unsaved edits survive switching views.

import { SvelteSet } from 'svelte/reactivity';
import {
	api,
	ApiError,
	ART,
	type ArtRules,
	type FolderStatus,
	type SetupResponse,
	type TagCoverage,
	type TagGroup,
	type TagKind
} from './api';
import { toast } from './toast.svelte';

class SetupStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
	groups = $state<TagGroup[]>([]);
	kinds = $state<TagKind[]>([]);
	essentials = $state<string[]>([]);
	albumKinds = $state<string[]>([]);
	notEditable = $state<string[]>([]);
	suggestions = $state<string[]>([]);
	variables = $state<{ id: string; label: string; group: string }[]>([]);
	artFileNames = $state<string[]>([]);
	coverage = $state<TagCoverage | null>(null);

	/** As last saved on the daemon. */
	saved = $state({
		required: [] as string[],
		folder: '',
		structure: '',
		art: { required: false, placement: 'file', file_name: 'cover' } as ArtRules
	});
	/** Being edited. */
	required = new SvelteSet<string>();
	folder = $state('');
	structure = $state('');
	art = $state<ArtRules>({ required: false, placement: 'file', file_name: 'cover' });
	/** The saved folder as the daemon sees it. */
	folderStatus = $state<FolderStatus | null>(null);
	/** Bumped on every save, so album flags can be refreshed. */
	version = $state(0);

	/** Tags added or removed, plus a changed folder, structure or art rule. */
	changes = $derived(
		this.saved.required.filter((id) => !this.required.has(id)).length +
			[...this.required].filter((id) => !this.saved.required.includes(id)).length +
			(this.folder.trim() !== this.saved.folder ? 1 : 0) +
			(this.structure.trim() !== this.saved.structure ? 1 : 0) +
			(this.art.required !== this.saved.art.required ? 1 : 0) +
			(this.art.placement !== this.saved.art.placement ? 1 : 0) +
			((this.art.file_name.trim() || 'cover') !== this.saved.art.file_name ? 1 : 0)
	);

	#loading: Promise<void> | null = null;

	load() {
		this.#loading ??= api
			.setup()
			.then((r) => {
				this.groups = r.groups;
				this.kinds = r.kinds;
				this.essentials = r.essentials;
				this.albumKinds = r.album_kinds;
				this.notEditable = r.not_editable;
				this.suggestions = r.suggestions;
				this.variables = r.variables;
				this.artFileNames = r.art_file_names;
				this.#apply(r);
				this.loaded = true;
			})
			.catch(() => {
				this.failed = true;
				this.#loading = null;
			});
		return this.#loading;
	}

	/** Re-read the saved setup (e.g. before an import), keeping unsaved edits. */
	async refresh() {
		if (!this.loaded) return this.load();
		try {
			const r = await api.setup();
			if (this.changes === 0) this.#apply(r);
			else this.folderStatus = r.folder;
		} catch {}
	}

	async loadCoverage() {
		try {
			this.coverage = await api.coverage();
		} catch {
			// Coverage is informational; the rules still work without it.
		}
	}

	#apply(r: SetupResponse) {
		const s = r.setup;
		this.saved = {
			required: s.rules.required_tags,
			folder: s.folder ?? '',
			structure: s.structure,
			art: s.rules.art
		};
		this.required.clear();
		for (const id of s.rules.required_tags) this.required.add(id);
		this.folder = this.saved.folder;
		this.structure = this.saved.structure;
		this.art = { ...s.rules.art };
		this.folderStatus = r.folder;
	}

	label(id: string) {
		if (id === ART) return 'Album art';
		return this.kinds.find((k) => k.id === id)?.label ?? id;
	}

	toggle(id: string, on = !this.required.has(id)) {
		if (on) this.required.add(id);
		else this.required.delete(id);
	}

	setAll(ids: string[], on: boolean) {
		for (const id of ids) this.toggle(id, on);
	}

	revert() {
		this.required.clear();
		for (const id of this.saved.required) this.required.add(id);
		this.folder = this.saved.folder;
		this.structure = this.saved.structure;
		this.art = { ...this.saved.art };
	}

	async save() {
		this.saving = true;
		try {
			// Send in catalog order; the daemon normalizes the same way.
			const ids = this.kinds.map((k) => k.id).filter((id) => this.required.has(id));
			const r = await api.saveSetup({
				rules: { required_tags: ids, art: { ...this.art, file_name: this.art.file_name.trim() } },
				folder: this.folder.trim() || null,
				structure: this.structure.trim()
			});
			this.#apply(r);
			this.version++;
			toast('Library setup saved', 'success');
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving the library setup failed', 'error', 6000);
		} finally {
			this.saving = false;
		}
	}
}

export const setup = new SetupStore();
