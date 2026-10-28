// Inbox setup: which folder is scanned, how files are ingested (autofill,
// smart suggestions), and which are hidden.

import {
	api,
	ApiError,
	type Autofill,
	type FolderStatus,
	type HiddenStats,
	type IngestMode,
	type InboxSetupResponse
} from './api';
import { toast } from './toast.svelte';

class InboxSetupStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
	unhiding = $state(false);
	/** As last saved on the daemon. */
	saved = $state({
		ingest: 'copy' as IngestMode,
		folder: '',
		autofill: null as Autofill | null,
		suggestions: false,
		preferEnglish: false,
		familyNameFirst: false
	});
	ingest = $state<IngestMode>('copy');
	folder = $state('');
	autofill = $state<Autofill | null>(null);
	suggestions = $state(false);
	preferEnglish = $state(false);
	familyNameFirst = $state(false);
	/** The saved AcoustID key's last characters; null if none is saved. */
	keyHint = $state<string | null>(null);
	/** A new AcoustID key to save; '' removes the saved one, null leaves it be. */
	keyDraft = $state<string | null>(null);
	/** The installed fpcalc's version, null if it's missing. */
	fpcalc = $state<string | null>(null);
	/** Bumped on every save, so album flags can be refreshed. */
	version = $state(0);
	/** The saved folder as the daemon sees it. */
	folderStatus = $state<FolderStatus | null>(null);
	hidden = $state<HiddenStats>({ files: 0, albums: 0 });

	changes = $derived(
		(this.ingest === this.saved.ingest ? 0 : 1) +
			(this.folder.trim() === this.saved.folder ? 0 : 1) +
			Object.keys(this.autofill ?? {}).filter(
				(k) => this.autofill?.[k as keyof Autofill] !== this.saved.autofill?.[k as keyof Autofill]
			).length +
			(this.suggestions === this.saved.suggestions ? 0 : 1) +
			(this.preferEnglish === this.saved.preferEnglish ? 0 : 1) +
			(this.familyNameFirst === this.saved.familyNameFirst ? 0 : 1) +
			(this.keyDraft === null ? 0 : 1)
	);
	/** Move deletes originals, so the Inbox folder must be writable. */
	canMove = $derived(this.folderStatus?.writable ?? false);

	#apply(r: InboxSetupResponse) {
		this.saved = {
			ingest: r.setup.ingest,
			folder: r.setup.folder ?? '',
			autofill: r.setup.autofill,
			suggestions: r.setup.suggestions.enabled,
			preferEnglish: r.setup.suggestions.prefer_english,
			familyNameFirst: r.setup.suggestions.family_name_first
		};
		this.ingest = this.saved.ingest;
		this.folder = this.saved.folder;
		this.autofill = { ...r.setup.autofill };
		this.suggestions = this.saved.suggestions;
		this.preferEnglish = this.saved.preferEnglish;
		this.familyNameFirst = this.saved.familyNameFirst;
		this.keyDraft = null;
		this.#status(r);
	}

	/** What the daemon owns, refreshed even over unsaved edits. */
	#status(r: InboxSetupResponse) {
		this.folderStatus = r.folder;
		this.hidden = r.hidden;
		this.keyHint = r.suggest.key_hint;
		this.fpcalc = r.suggest.fpcalc;
	}

	async load() {
		try {
			const r = await api.inboxSetup();
			if (this.loaded && this.changes) {
				// Keep unsaved choices; refresh only what the daemon owns.
				this.#status(r);
			} else {
				this.#apply(r);
			}
			this.loaded = true;
			this.failed = false;
		} catch {
			this.failed = !this.loaded;
		}
	}

	revert() {
		this.ingest = this.saved.ingest;
		this.folder = this.saved.folder;
		this.autofill = this.saved.autofill && { ...this.saved.autofill };
		this.suggestions = this.saved.suggestions;
		this.preferEnglish = this.saved.preferEnglish;
		this.familyNameFirst = this.saved.familyNameFirst;
		this.keyDraft = null;
	}

	/** Returns whether the Inbox folder changed (the daemon rescans then). */
	async save(): Promise<boolean> {
		this.saving = true;
		const before = this.saved.folder;
		try {
			this.#apply(
				await api.saveInboxSetup(
					{
						ingest: this.ingest,
						folder: this.folder.trim() || null,
						autofill: this.autofill!,
						suggestions: {
							enabled: this.suggestions,
							prefer_english: this.preferEnglish,
							family_name_first: this.familyNameFirst
						}
					},
					this.keyDraft?.trim() ?? undefined
				)
			);
			this.version++;
			toast('Inbox setup saved', 'success');
			return this.saved.folder !== before;
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
			return false;
		} finally {
			this.saving = false;
		}
	}

	/** Returns how many files came back, or null if it failed. */
	async unhideAll(): Promise<number | null> {
		this.unhiding = true;
		try {
			const r = await api.unhideAll();
			this.hidden = r.hidden;
			return r.unhidden;
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Unhiding failed', 'error', 6000);
			return null;
		} finally {
			this.unhiding = false;
		}
	}
}

export const inboxSetup = new InboxSetupStore();
