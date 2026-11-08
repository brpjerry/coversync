// The devices paired with this server: listed in the sidebar's Sync section
// and in Sync setup, where they're added, renamed, re-scoped and revoked.
// Changes are made at once, not through the save bar.

import { api, ApiError, type Device, type Scope } from './api';
import { toast } from './toast.svelte';

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
		} catch {
			if (version === this.#version) this.failed = !this.loaded;
		}
	}

	#put(device: Device) {
		const i = this.list.findIndex((d) => d.id === device.id);
		if (i >= 0) this.list[i] = device;
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
			// It may be gone, or changed elsewhere.
			if (e instanceof ApiError && e.status === 404) this.load();
			return false;
		} finally {
			delete this.busy[id];
		}
	}

	update(id: number, change: { name?: string; scope?: Scope }) {
		return this.#change(id, () => api.updateDevice(id, change), 'Saving the device failed');
	}

	revoke(id: number) {
		return this.#change(id, () => api.revokeDevice(id), 'Revoking the device failed');
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
export const platformName = (p: string) => PLATFORMS[p.toLowerCase()] ?? p;

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

/** "just now", "5 minutes ago", "3 days ago", or the date for older ones. */
export function ago(seconds: number, now = Date.now() / 1000) {
	const d = Math.max(0, now - seconds);
	if (d < 60) return 'just now';
	const unit = (n: number, what: string) => `${n} ${what}${n === 1 ? '' : 's'} ago`;
	if (d < 3600) return unit(Math.floor(d / 60), 'minute');
	if (d < 86400) return unit(Math.floor(d / 3600), 'hour');
	if (d < 30 * 86400) return unit(Math.floor(d / 86400), 'day');
	return new Date(seconds * 1000).toLocaleDateString(undefined, { dateStyle: 'medium' });
}
