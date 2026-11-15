import { describe, expect, it, vi } from 'vitest';
import type { DeviceSyncState } from './api';

/** Sync state reads, each answered when the test says. */
const reads: { id: number; answer: (state: Partial<DeviceSyncState>) => void }[] = [];

vi.mock('./api', () => ({
	ApiError: class extends Error {},
	api: {
		deviceSyncState: (id: number) => new Promise((resolve) => reads.push({ id, answer: resolve })),
		deviceSyncAction: () => Promise.resolve()
	}
}));
vi.mock('./toast.svelte', () => ({ toast: () => {} }));

const { devices } = await import('./devices.svelte');

/** Let promise callbacks run. */
const settle = () => new Promise((resolve) => setTimeout(resolve));

describe('device sync reads', () => {
	it('go one at a time per device, so each lands over an older one', async () => {
		const stop = devices.watchSync();
		// A notice a second while a read is slow: one read under way, one waiting.
		void devices.readSync(1);
		await settle();
		void devices.readSync(1);
		void devices.readSync(1);
		await settle();
		expect(reads).toHaveLength(1);

		// An action taken during the first read ends only once a read started
		// after it has landed: the waiting one.
		let acted = false;
		const action = devices.syncAction(1, 'sync').then(() => (acted = true));
		await settle();
		expect(reads).toHaveLength(1);
		reads[0].answer({ sync_requested: false });
		await settle();
		expect(reads).toHaveLength(2);
		expect(devices.syncStates[1]?.sync_requested).toBe(false);
		expect(acted).toBe(false);
		reads[1].answer({ sync_requested: true });
		await action;
		expect(devices.syncStates[1]?.sync_requested).toBe(true);
		expect(devices.syncBusy[1]).toBeUndefined();

		// Closed, nothing more is read and what was shown goes.
		stop();
		await devices.readSync(1);
		expect(reads).toHaveLength(2);
		expect(devices.syncStates).toEqual({});
	});
});
