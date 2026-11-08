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
import { SetupStore } from './setup-store.svelte';
import { toast } from './toast.svelte';

class LibrarySetupStore extends SetupStore<SetupResponse> {
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
		art: { required: false, placement: 'file', file_name: 'cover' } as ArtRules,
		separator: ';'
	});
	/** Being edited. */
	required = new SvelteSet<string>();
	folder = $state('');
	structure = $state('');
	art = $state<ArtRules>({ required: false, placement: 'file', file_name: 'cover' });
	/** Kept as typed, spaces included: "; " isn't ";". */
	separator = $state(';');
	/** The saved folder as the daemon sees it. */
	folderStatus = $state<FolderStatus | null>(null);
	/** Why the saved folder can't hold the library's database, if it can't. */
	libraryProblem = $state<string | null>(null);
	/** Bumped on every save, so album flags can be refreshed. */
	version = $state(0);

	/** Tags added or removed, plus a changed folder, structure, art rule or separator. */
	changes = $derived(
		this.saved.required.filter((id) => !this.required.has(id)).length +
			[...this.required].filter((id) => !this.saved.required.includes(id)).length +
			(this.folder.trim() !== this.saved.folder ? 1 : 0) +
			(this.structure.trim() !== this.saved.structure ? 1 : 0) +
			(this.art.required !== this.saved.art.required ? 1 : 0) +
			(this.art.placement !== this.saved.art.placement ? 1 : 0) +
			((this.art.file_name.trim() || 'cover') !== this.saved.art.file_name ? 1 : 0) +
			(this.separator !== this.saved.separator ? 1 : 0)
	);

	protected fetch() {
		return api.setup();
	}

	protected take(r: SetupResponse, first: boolean) {
		if (first) {
			// The tag catalog comes once.
			this.groups = r.groups;
			this.kinds = r.kinds;
			this.essentials = r.essentials;
			this.albumKinds = r.album_kinds;
			this.notEditable = r.not_editable;
			this.suggestions = r.suggestions;
			this.variables = r.variables;
			this.artFileNames = r.art_file_names;
		}
		// With unsaved edits, the setup as saved (elsewhere, say) still moves
		// under them, so Revert goes back to it.
		if (first || this.changes === 0) this.#apply(r);
		else this.#rebase(r);
	}

	async loadCoverage() {
		try {
			this.coverage = await api.coverage();
		} catch {
			// Coverage is informational; the rules still work without it.
		}
	}

	/** The setup as saved, and what the daemon owns, leaving edits be. */
	#rebase(r: SetupResponse) {
		const s = r.setup;
		this.saved = {
			required: s.rules.required_tags,
			folder: s.folder ?? '',
			structure: s.structure,
			art: s.rules.art,
			separator: s.rules.separator
		};
		this.folderStatus = r.folder;
		this.libraryProblem = r.library_problem;
	}

	#apply(r: SetupResponse) {
		const s = r.setup;
		this.#rebase(r);
		this.required.clear();
		for (const id of s.rules.required_tags) this.required.add(id);
		this.folder = this.saved.folder;
		this.structure = this.saved.structure;
		this.art = { ...s.rules.art };
		this.separator = s.rules.separator;
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
		this.separator = this.saved.separator;
	}

	save() {
		return this.write(async () => {
			try {
				// Send in catalog order; the daemon normalizes the same way.
				const ids = this.kinds.map((k) => k.id).filter((id) => this.required.has(id));
				const r = await api.saveSetup({
					rules: {
						required_tags: ids,
						art: { ...this.art, file_name: this.art.file_name.trim() },
						separator: this.separator
					},
					folder: this.folder.trim() || null,
					structure: this.structure.trim()
				});
				this.#apply(r);
				this.version++;
				if (r.adopted)
					toast(`Opened the library in ${r.setup.folder}, with its own rules`, 'info', 6000);
				else toast('Library setup saved', 'success');
			} catch (e) {
				toast(e instanceof ApiError ? e.message : 'Saving the library setup failed', 'error', 6000);
			}
		});
	}
}

export const setup = new LibrarySetupStore();
