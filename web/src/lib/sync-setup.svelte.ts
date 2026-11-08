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
import { toast } from './toast.svelte';

class SyncSetupStore {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);
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
	/** Bumped by each save, so a load that started before one can't undo it. */
	#saves = 0;

	#applySync(r: SyncSetupResponse, keepEdits: boolean) {
		if (!keepEdits || this.enabled === this.savedEnabled) this.enabled = r.setup.enabled;
		this.savedEnabled = r.setup.enabled;
	}

	#applyPairing(r: PairingSetupResponse, keepEdits: boolean) {
		const path = r.setup.ca_path ?? '';
		if (!keepEdits || this.caPath.trim() === this.savedCaPath) this.caPath = path;
		if (!keepEdits || this.allowHttp === this.savedAllowHttp) this.allowHttp = r.setup.allow_http;
		this.savedCaPath = path;
		this.savedAllowHttp = r.setup.allow_http;
		this.ca = r.ca;
		this.caProblem = r.ca_problem;
	}

	async load() {
		const saves = this.#saves;
		try {
			const [sync, pairing] = await Promise.all([api.syncSetup(), api.pairingSetup()]);
			if (saves !== this.#saves) return;
			// Keep unsaved choices.
			this.#applySync(sync, this.loaded);
			this.#applyPairing(pairing, this.loaded);
			this.loaded = true;
			this.failed = false;
		} catch {
			this.failed = !this.loaded;
		}
	}

	revert() {
		this.enabled = this.savedEnabled;
		this.caPath = this.savedCaPath;
		this.allowHttp = this.savedAllowHttp;
		this.caRefused = null;
	}

	/** Save what changed; returns whether all of it was saved. */
	async save() {
		this.saving = true;
		this.#saves++;
		// What's saved is what was on screen at the click: changes made while
		// saving stay, unsaved.
		const enabled = this.enabled;
		const caPath = this.caPath.trim();
		const allowHttp = this.allowHttp;
		let ok = true;
		try {
			if (enabled !== this.savedEnabled) {
				try {
					const r = await api.saveSyncSetup({ enabled });
					// A choice made while saving stays, unsaved.
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
					if (this.caPath.trim() === caPath) this.caPath = r.setup.ca_path ?? '';
					if (this.allowHttp === allowHttp) this.allowHttp = r.setup.allow_http;
					this.savedCaPath = r.setup.ca_path ?? '';
					this.savedAllowHttp = r.setup.allow_http;
					this.ca = r.ca;
					this.caProblem = r.ca_problem;
					if (this.caRefused?.path === caPath) this.caRefused = null;
				} catch (e) {
					ok = false;
					if (e instanceof ApiError && e.code === 'bad_ca')
						this.caRefused = { path: caPath, why: e.message, at: Date.now() };
					toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
				}
			}
			if (ok) toast('Sync setup saved', 'success');
		} finally {
			this.saving = false;
		}
		return ok;
	}
}

export const syncSetup = new SyncSetupStore();
