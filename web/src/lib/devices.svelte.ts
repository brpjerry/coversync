// The devices paired with this server: listed in the sidebar's Sync section
// and in Sync setup, where they're added, renamed, re-scoped and revoked, and
// their sync settings changed. Changes are made at once, not through the
// save bar. While Sync setup is open, each device's sync is read too, and
// read again as it moves on.

import { createSubscriber } from 'svelte/reactivity';
import {
	api,
	ApiError,
	type Device,
	type DeviceSettings,
	type DeviceSyncState,
	type Scope,
	type SyncAction
} from './api';
import { toast } from './toast.svelte';

const ACTION_FAILED: Record<SyncAction, string> = {
	sync: 'Asking the device to sync failed',
	check: 'Asking for a check of its files failed',
	retry: 'Retrying the failed files failed',
	remove_other: 'Asking to remove the files failed',
	keep_other: 'Asking to keep the files failed'
};

class DevicesStore {
	loaded = $state(false);
	failed = $state(false);
	list = $state<Device[]>([]);
	/** The device this page is signed in as, through its token. */
	thisDevice = $state<number | null>(null);
	/** Signed in with the password: only that may change devices. */
	canManage = $state(false);
	/** Devices whose change is being saved. */
	busy = $state<Record<number, boolean>>({});

	active = $derived(this.list.filter((d) => d.revoked_at === null));
	/**
	 * Bumped by each load and each change: a load answered after a change
	 * started may have read the list before it, so it's dropped.
	 */
	#version = 0;

	async load() {
		const version = ++this.#version;
		try {
			const r = await api.devices();
			if (version !== this.#version) return;
			this.list = r.devices;
			this.thisDevice = r.this_device;
			this.canManage = r.can_manage;
			this.loaded = true;
			this.failed = false;
			// Devices may have come or gone, and saved settings changed what's left to sync.
			this.readSyncAll();
		} catch {
			if (version === this.#version) this.failed = !this.loaded;
		}
	}

	/**
	 * Each device's sync, while Sync setup shows it (see `watchSync`): the
	 * devices whose sync settings this login may read, which are the ones
	 * whose sync it may read.
	 */
	syncStates = $state<Record<number, DeviceSyncState>>({});
	/** Devices whose sync action is being sent, and which. */
	syncBusy = $state<Record<number, SyncAction>>({});
	/** Screens showing sync states. */
	#watching = 0;
	/** Per device: the read under way, and the one waiting for it to end. */
	#reading = new Map<number, Promise<void>>();
	#waiting = new Map<number, Promise<void>>();

	/** Read every device's sync while a screen shows it; returns the stop. */
	watchSync() {
		this.#watching++;
		this.readSyncAll();
		return () => {
			// Not kept for next time, when it would show out of date at first.
			if (!--this.#watching) this.syncStates = {};
		};
	}

	/** Read the sync of each device this login may read again, while it's shown. */
	readSyncAll() {
		if (!this.#watching) return;
		const ids = this.list.filter((d) => d.sync && d.revoked_at === null).map((d) => d.id);
		for (const id of Object.keys(this.syncStates).map(Number))
			if (!ids.includes(id)) delete this.syncStates[id];
		for (const id of ids) void this.readSync(id);
	}

	/**
	 * Read a device's sync again, while it's shown. A device's reads go one
	 * at a time, so each lands over an older one, and a busy device (whose
	 * sync moves on every second) isn't asked more often than it answers:
	 * asked during a read, the next starts when it ends, and asks meanwhile
	 * share that one. Resolves once a read started after the ask has landed.
	 */
	readSync(id: number): Promise<void> {
		if (!this.#watching) return Promise.resolve();
		const waiting = this.#waiting.get(id);
		if (waiting) return waiting;
		const next = (this.#reading.get(id) ?? Promise.resolve()).then(() => {
			this.#waiting.delete(id);
			const read = this.#readState(id);
			this.#reading.set(id, read);
			return read.finally(() => {
				if (this.#reading.get(id) === read) this.#reading.delete(id);
			});
		});
		this.#waiting.set(id, next);
		return next;
	}

	/** Never fails: a read that does leaves what's shown. */
	async #readState(id: number) {
		try {
			const state = await api.deviceSyncState(id);
			if (this.#watching) this.syncStates[id] = state;
		} catch (e) {
			// Gone, or no longer this login's to read.
			if (e instanceof ApiError && (e.status === 403 || e.status === 404))
				delete this.syncStates[id];
		}
	}

	/** Ask something of a device's sync (a password login only); returns whether it was taken. */
	async syncAction(id: number, action: SyncAction) {
		this.syncBusy[id] = action;
		try {
			await api.deviceSyncAction(id, action);
			// Shown as it is now before the button comes back.
			await this.readSync(id);
			return true;
		} catch (e) {
			toast(e instanceof ApiError ? e.message : ACTION_FAILED[action], 'error', 6000);
			return false;
		} finally {
			delete this.syncBusy[id];
		}
	}

	#put(device: Device) {
		const i = this.list.findIndex((d) => d.id === device.id);
		// A change's answer leaves out the sync settings, which stay.
		if (i >= 0) this.list[i] = { ...this.list[i], ...device };
		else this.list.push(device);
	}

	/** Run a change to one device; says what went wrong, and returns whether it worked. */
	async #change(id: number, run: () => Promise<Device | void>, failed: string) {
		this.busy[id] = true;
		this.#version++;
		try {
			const device = await run();
			this.#version++;
			if (device) this.#put(device);
			return true;
		} catch (e) {
			toast(e instanceof ApiError ? e.message : failed, 'error', 6000);
			return false;
		} finally {
			delete this.busy[id];
			// A load answered during the change was dropped, and the device
			// may be gone or changed elsewhere: read the list again.
			void this.load();
		}
	}

	update(id: number, change: { name?: string; scope?: Scope }) {
		return this.#change(id, () => api.updateDevice(id, change), 'Saving the device failed');
	}

	revoke(id: number) {
		return this.#change(id, () => api.revokeDevice(id), 'Revoking the device failed');
	}

	saveSync(id: number, settings: DeviceSettings) {
		return this.#change(
			id,
			async () => {
				const sync = await api.saveDeviceSync(id, settings);
				const device = this.list.find((d) => d.id === id);
				// Not over newer settings a reload already brought.
				if (device && (!device.sync || device.sync.version <= sync.version)) device.sync = sync;
			},
			'Saving the sync settings failed'
		);
	}

	remove(id: number) {
		return this.#change(
			id,
			async () => {
				await api.removeDevice(id);
				this.list = this.list.filter((d) => d.id !== id);
			},
			'Removing the device failed'
		);
	}
}

export const devices = new DevicesStore();

const PLATFORMS: Record<string, string> = {
	android: 'Android',
	ios: 'iOS',
	linux: 'Linux',
	windows: 'Windows',
	macos: 'macOS',
	rockbox: 'Rockbox'
};

/** "Android" for "android"; anything else as the device said it. */
export function platformName(p: string) {
	const key = p.toLowerCase();
	// Only the names listed: not "constructor" or "__proto__" from the object itself.
	return Object.hasOwn(PLATFORMS, key) ? PLATFORMS[key] : p;
}

export const SCOPES: Record<Scope, { label: string; description: string }> = {
	manage: {
		label: 'Manage and sync',
		description:
			'The desktop app or your phone: everything this page can do, except managing devices.'
	},
	sync: {
		label: 'Sync only',
		description: 'A music player: only its own files and its status.'
	}
};

/**
 * A minute's tick for relative times: one timer, running only while some
 * effect or template is reading one.
 */
const minutes = createSubscriber((update) => {
	const timer = setInterval(update, 60_000);
	return () => clearInterval(timer);
});

/** "just now", "5 minutes ago", "3 days ago", or the date for older ones. */
export function ago(seconds: number) {
	// Shown again each minute, while anything shows one.
	minutes();
	const d = Math.max(0, Date.now() / 1000 - seconds);
	if (d < 60) return 'just now';
	const unit = (n: number, what: string) => `${n} ${what}${n === 1 ? '' : 's'} ago`;
	if (d < 3600) return unit(Math.floor(d / 60), 'minute');
	if (d < 86400) return unit(Math.floor(d / 3600), 'hour');
	if (d < 30 * 86400) return unit(Math.floor(d / 86400), 'day');
	return new Date(seconds * 1000).toLocaleDateString(undefined, { dateStyle: 'medium' });
}
