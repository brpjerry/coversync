import { afterEach, describe, expect, it, vi } from 'vitest';
import { readPref, writePref } from './browse';

const isString = (v: unknown): v is string => typeof v === 'string';

describe('preferences', () => {
	afterEach(() => vi.unstubAllGlobals());

	it('keeps one storage refuses for the page, until a write succeeds', () => {
		const stored = new Map<string, string>();
		let full = true;
		vi.stubGlobal('localStorage', {
			getItem: (k: string) => stored.get(k) ?? null,
			setItem: (k: string, v: string) => {
				if (full) throw new DOMException('full', 'QuotaExceededError');
				stored.set(k, v);
			}
		});
		writePref('coversync.test', 'Anime');
		expect(readPref('coversync.test', isString, '')).toBe('Anime');
		// Picking None is a value too.
		writePref('coversync.test', '');
		expect(readPref('coversync.test', isString, 'x')).toBe('');
		full = false;
		writePref('coversync.test', 'Love Live');
		// Another tab's change shows once this page's is saved.
		stored.set('coversync.test', 'Other tab');
		expect(readPref('coversync.test', isString, '')).toBe('Other tab');
	});
});
