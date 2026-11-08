// Sync setup: for now just whether sync is on, which shows the sidebar's Sync
// section. Loaded at start-up because the sidebar needs it.

import { api, ApiError, type SyncSetupResponse } from './api';
import { SetupStore } from './setup-store.svelte';
import { toast } from './toast.svelte';

class SyncSetupStore extends SetupStore<SyncSetupResponse> {
	/** As last saved on the daemon. */
	savedEnabled = $state(true);
	/** Being edited. */
	enabled = $state(true);

	changes = $derived(this.enabled === this.savedEnabled ? 0 : 1);

	protected fetch() {
		return api.syncSetup();
	}

	protected take(r: SyncSetupResponse, first: boolean) {
		// Keep an unsaved choice.
		if (!first && this.changes) this.savedEnabled = r.setup.enabled;
		else this.savedEnabled = this.enabled = r.setup.enabled;
	}

	revert() {
		this.enabled = this.savedEnabled;
	}

	save() {
		const enabled = this.enabled;
		return this.write(async () => {
			try {
				const r = await api.saveSyncSetup({ enabled });
				this.savedEnabled = r.setup.enabled;
				// A choice made while saving stays, unsaved.
				if (this.enabled === enabled) this.enabled = r.setup.enabled;
				toast('Sync setup saved', 'success');
			} catch (e) {
				toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
			}
		});
	}
}

export const syncSetup = new SyncSetupStore();
