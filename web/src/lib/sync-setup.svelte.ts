// Sync setup: for now just whether sync is on, which shows the sidebar's Sync
// section. Loaded at start-up because the sidebar needs it.

import { api, ApiError, type SyncSetupResponse } from './api';
import { toast } from './toast.svelte';

class SyncSetupStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
	/** As last saved on the daemon. */
	savedEnabled = $state(true);
	/** Being edited. */
	enabled = $state(true);

	changes = $derived(this.enabled === this.savedEnabled ? 0 : 1);
	/** Bumped by each read, so only the newest one lands. */
	#reads = 0;
	/** Bumped as each save starts and ends, so no read sent before or during one lands. */
	#saves = 0;

	#apply(r: SyncSetupResponse) {
		this.savedEnabled = this.enabled = r.setup.enabled;
	}

	async load() {
		const read = ++this.#reads;
		const saves = this.#saves;
		try {
			const r = await api.syncSetup();
			if (read !== this.#reads || saves !== this.#saves) return;
			// Keep an unsaved choice.
			if (this.loaded && this.changes) this.savedEnabled = r.setup.enabled;
			else this.#apply(r);
			this.loaded = true;
			this.failed = false;
		} catch {
			if (read === this.#reads) this.failed = !this.loaded;
		}
	}

	revert() {
		this.enabled = this.savedEnabled;
	}

	async save() {
		this.saving = true;
		this.#saves++;
		const reads = this.#reads;
		const enabled = this.enabled;
		try {
			const r = await api.saveSyncSetup({ enabled });
			this.savedEnabled = r.setup.enabled;
			// A choice made while saving stays, unsaved.
			if (this.enabled === enabled) this.enabled = r.setup.enabled;
			toast('Sync setup saved', 'success');
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
		} finally {
			this.saving = false;
			this.#saves++;
			// Reads asked for meanwhile (a save elsewhere, say) were dropped: read again.
			if (this.#reads !== reads) void this.load();
		}
	}
}

export const syncSetup = new SyncSetupStore();
