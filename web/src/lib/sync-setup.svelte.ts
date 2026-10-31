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

	#apply(r: SyncSetupResponse) {
		this.savedEnabled = this.enabled = r.setup.enabled;
	}

	async load() {
		try {
			const r = await api.syncSetup();
			// Keep an unsaved choice.
			if (this.loaded && this.changes) this.savedEnabled = r.setup.enabled;
			else this.#apply(r);
			this.loaded = true;
			this.failed = false;
		} catch {
			this.failed = !this.loaded;
		}
	}

	revert() {
		this.enabled = this.savedEnabled;
	}

	async save() {
		this.saving = true;
		try {
			this.#apply(await api.saveSyncSetup({ enabled: this.enabled }));
			toast('Sync setup saved', 'success');
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
		} finally {
			this.saving = false;
		}
	}
}

export const syncSetup = new SyncSetupStore();
