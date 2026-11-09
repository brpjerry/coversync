// Inbox setup: which folder is scanned, where uploads go, how files are
// ingested (autofill, smart suggestions), and which are hidden.

import {
	api,
	ApiError,
	type Autofill,
	type FolderStatus,
	type HiddenStats,
	type IngestMode,
	type InboxSetupResponse
} from './api';
import { SetupStore } from './setup-store.svelte';
import { toast } from './toast.svelte';

class InboxSetupStore extends SetupStore<InboxSetupResponse> {
	unhiding = $state(false);
	/** As last saved on the daemon. */
	saved = $state({
		ingest: 'copy' as IngestMode,
		folder: '',
		uploads: '',
		autofill: null as Autofill | null,
		suggestions: false,
		preferEnglish: false,
		familyNameFirst: false,
		saveLookups: true
	});
	ingest = $state<IngestMode>('copy');
	folder = $state('');
	/** '' means Uploads in the Inbox folder. */
	uploads = $state('');
	autofill = $state<Autofill | null>(null);
	suggestions = $state(false);
	preferEnglish = $state(false);
	familyNameFirst = $state(false);
	saveLookups = $state(true);
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
			(this.uploads.trim() === this.saved.uploads ? 0 : 1) +
			Object.keys(this.autofill ?? {}).filter(
				(k) => this.autofill?.[k as keyof Autofill] !== this.saved.autofill?.[k as keyof Autofill]
			).length +
			(this.suggestions === this.saved.suggestions ? 0 : 1) +
			(this.preferEnglish === this.saved.preferEnglish ? 0 : 1) +
			(this.familyNameFirst === this.saved.familyNameFirst ? 0 : 1) +
			(this.saveLookups === this.saved.saveLookups ? 0 : 1) +
			(this.keyDraft === null ? 0 : 1)
	);
	/** Move deletes originals, so the Inbox folder must be writable. */
	canMove = $derived(this.folderStatus?.writable ?? false);

	/** The setup as saved, and what the daemon owns, leaving edits be. */
	#rebase(r: InboxSetupResponse) {
		const was = this.saved;
		this.saved = {
			ingest: r.setup.ingest,
			folder: r.setup.folder ?? '',
			uploads: r.setup.uploads ?? '',
			autofill: r.setup.autofill,
			suggestions: r.setup.suggestions.enabled,
			preferEnglish: r.setup.suggestions.prefer_english,
			familyNameFirst: r.setup.suggestions.family_name_first,
			saveLookups: r.setup.suggestions.save_lookups
		};
		// Fields not edited here follow what's saved.
		const now = this.saved;
		if (this.ingest === was.ingest) this.ingest = now.ingest;
		if (this.folder.trim() === was.folder) this.folder = now.folder;
		if (this.uploads.trim() === was.uploads) this.uploads = now.uploads;
		if (this.suggestions === was.suggestions) this.suggestions = now.suggestions;
		if (this.preferEnglish === was.preferEnglish) this.preferEnglish = now.preferEnglish;
		if (this.familyNameFirst === was.familyNameFirst) this.familyNameFirst = now.familyNameFirst;
		if (this.saveLookups === was.saveLookups) this.saveLookups = now.saveLookups;
		if (this.autofill && now.autofill)
			for (const key of Object.keys(now.autofill) as (keyof Autofill)[])
				if (this.autofill[key] === was.autofill?.[key]) this.autofill[key] = now.autofill[key];
		this.#status(r);
	}

	#apply(r: InboxSetupResponse) {
		this.#rebase(r);
		this.ingest = this.saved.ingest;
		this.folder = this.saved.folder;
		this.uploads = this.saved.uploads;
		this.autofill = { ...r.setup.autofill };
		this.suggestions = this.saved.suggestions;
		this.preferEnglish = this.saved.preferEnglish;
		this.familyNameFirst = this.saved.familyNameFirst;
		this.saveLookups = this.saved.saveLookups;
		this.keyDraft = null;
	}

	/** What the daemon owns, refreshed even over unsaved edits. */
	#status(r: InboxSetupResponse) {
		this.folderStatus = r.folder;
		this.hidden = r.hidden;
		this.keyHint = r.suggest.key_hint;
		this.fpcalc = r.suggest.fpcalc;
	}

	protected fetch() {
		return api.inboxSetup();
	}

	protected take(r: InboxSetupResponse, first: boolean) {
		// Keep unsaved choices; what's saved (elsewhere, say) still moves under
		// them, so Revert goes back to it.
		if (!first && this.changes) this.#rebase(r);
		else this.#apply(r);
	}

	revert() {
		this.ingest = this.saved.ingest;
		this.folder = this.saved.folder;
		this.uploads = this.saved.uploads;
		this.autofill = this.saved.autofill && { ...this.saved.autofill };
		this.suggestions = this.saved.suggestions;
		this.preferEnglish = this.saved.preferEnglish;
		this.familyNameFirst = this.saved.familyNameFirst;
		this.saveLookups = this.saved.saveLookups;
		this.keyDraft = null;
	}

	/** Returns whether the Inbox or uploads folder changed (the daemon rescans then). */
	save(): Promise<boolean> {
		const before = [this.saved.folder, this.saved.uploads];
		return this.write(async () => {
			try {
				this.#apply(
					await api.saveInboxSetup(
						{
							ingest: this.ingest,
							folder: this.folder.trim() || null,
							uploads: this.uploads.trim() || null,
							autofill: this.autofill!,
							suggestions: {
								enabled: this.suggestions,
								prefer_english: this.preferEnglish,
								family_name_first: this.familyNameFirst,
								save_lookups: this.saveLookups
							}
						},
						this.keyDraft?.trim() ?? undefined
					)
				);
				this.version++;
				toast('Inbox setup saved', 'success');
				return this.saved.folder !== before[0] || this.saved.uploads !== before[1];
			} catch (e) {
				toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
				return false;
			}
		});
	}

	/** Returns how many files came back, or null if it failed. */
	async unhideAll(): Promise<number | null> {
		this.unhiding = true;
		try {
			// Hidden counts read meanwhile could be from before.
			return await this.write(async () => {
				const r = await api.unhideAll();
				this.hidden = r.hidden;
				return r.unhidden;
			}, true);
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Unhiding failed', 'error', 6000);
			return null;
		} finally {
			this.unhiding = false;
		}
	}
}

export const inboxSetup = new InboxSetupStore();
