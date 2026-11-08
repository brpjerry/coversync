// Sync setup: whether sync is on, which shows the sidebar's Sync section, and
// how devices pair (the CA certificate for apps, plain http). Loaded at
// start-up because the sidebar needs it.

import {
	api,
	ApiError,
	type CaInfo,
	type PairingSetupResponse,
	type SyncSetupResponse
} from './api';
import { SetupStore } from './setup-store.svelte';
import { toast } from './toast.svelte';

/** Sync setup's two halves, read together. */
interface SyncAndPairing {
	sync: SyncSetupResponse;
	pairing: PairingSetupResponse;
}

class SyncSetupStore extends SetupStore<SyncAndPairing> {
	/** As last saved on the daemon. */
	savedEnabled = $state(true);
	/** Being edited. */
	enabled = $state(true);

	/** The CA certificate's path as saved, and as typed. */
	savedCaPath = $state('');
	caPath = $state('');
	savedAllowHttp = $state(false);
	allowHttp = $state(false);
	/** The saved certificate, when it can be used; otherwise why not. */
	ca = $state<CaInfo | null>(null);
	caProblem = $state<string | null>(null);
	/** The CA path saving refused, and why: shown while that path is typed. */
	caRefused = $state<{ path: string; why: string; at: number } | null>(null);

	changes = $derived(
		Number(this.enabled !== this.savedEnabled) +
			Number(this.caPath.trim() !== this.savedCaPath) +
			Number(this.allowHttp !== this.savedAllowHttp)
	);

	protected async fetch() {
		const [sync, pairing] = await Promise.all([api.syncSetup(), api.pairingSetup()]);
		return { sync, pairing };
	}

	/** A field left as it was saved follows the saved setup; an edited one stays. */
	protected take(r: SyncAndPairing, first: boolean) {
		if (first || this.enabled === this.savedEnabled) this.enabled = r.sync.setup.enabled;
		this.savedEnabled = r.sync.setup.enabled;
		this.#takePairing(r.pairing, first);
	}

	#takePairing(r: PairingSetupResponse, first: boolean) {
		const path = r.setup.ca_path ?? '';
		if (first || this.caPath.trim() === this.savedCaPath) this.caPath = path;
		if (first || this.allowHttp === this.savedAllowHttp) this.allowHttp = r.setup.allow_http;
		this.savedCaPath = path;
		this.savedAllowHttp = r.setup.allow_http;
		this.ca = r.ca;
		this.caProblem = r.ca_problem;
	}

	revert() {
		this.enabled = this.savedEnabled;
		this.caPath = this.savedCaPath;
		this.allowHttp = this.savedAllowHttp;
	}

	/**
	 * Save what changed, as it was on screen at the click (changes made while
	 * saving stay, unsaved); returns whether all of it was saved.
	 */
	save() {
		const enabled = this.enabled;
		const caPath = this.caPath.trim();
		const allowHttp = this.allowHttp;
		return this.write(async () => {
			let ok = true;
			if (enabled !== this.savedEnabled) {
				try {
					const r = await api.saveSyncSetup({ enabled });
					if (this.enabled === enabled) this.enabled = r.setup.enabled;
					this.savedEnabled = r.setup.enabled;
				} catch (e) {
					ok = false;
					toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
				}
			}
			if (caPath !== this.savedCaPath || allowHttp !== this.savedAllowHttp) {
				try {
					const r = await api.savePairingSetup({ ca_path: caPath || null, allow_http: allowHttp });
					// As for a read, but what was just saved replaces what was sent.
					if (this.caPath.trim() === caPath) this.caPath = r.setup.ca_path ?? '';
					if (this.allowHttp === allowHttp) this.allowHttp = r.setup.allow_http;
					this.#takePairing(r, false);
					if (this.caRefused?.path === caPath) this.caRefused = null;
				} catch (e) {
					ok = false;
					if (e instanceof ApiError && e.code === 'bad_ca')
						this.caRefused = { path: caPath, why: e.message, at: Date.now() };
					toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
				}
			}
			if (ok) toast('Sync setup saved', 'success');
			return ok;
		});
	}
}

export const syncSetup = new SyncSetupStore();
