// How the album grid is browsed: which collection, in which order.

/**
 * The inbox is the scanned on-disk files; the library is what coversync
 * manages. Each has a setup screen, and appearance covers the browser itself.
 */
export type View = 'inbox' | 'inbox-setup' | 'library' | 'library-setup' | 'appearance';
export const VIEWS: View[] = ['inbox', 'inbox-setup', 'library', 'library-setup', 'appearance'];

/** The view named in a URL's `?view=`; `setup` was Library setup's first name. */
export function parseView(v: string | null): View {
	if (v === 'setup') return 'library-setup';
	return VIEWS.includes(v as View) ? (v as View) : 'inbox';
}

/** The collection (top tab) a view belongs to, if any. */
export function collection(v: View): 'inbox' | 'library' | null {
	if (v === 'appearance') return null;
	return v.startsWith('library') ? 'library' : 'inbox';
}
export const isSetup = (v: View) => v.endsWith('-setup') || v === 'appearance';

/** A sort tab: a tag kind id, or 'added'. Which ones exist is set in Appearance. */
export type Sort = string;

/** Earlier builds stored these names for the original four tabs. */
const OLD_SORTS: Record<string, Sort> = {
	artist: 'album_artist',
	title: 'album',
	year: 'date'
};
export const migrateSort = (s: string) => OLD_SORTS[s] ?? s;

/** A localStorage-backed preference; storage can be unavailable, so failures are ignored. */
export function readPref<T>(key: string, valid: (v: unknown) => v is T, fallback: T): T {
	try {
		const v = localStorage.getItem(key);
		return valid(v) ? v : fallback;
	} catch {
		return fallback;
	}
}

export function writePref(key: string, value: string) {
	try {
		localStorage.setItem(key, value);
	} catch {}
}
