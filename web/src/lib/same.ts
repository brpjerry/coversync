// Comparing JSON answers from the daemon.

/**
 * Whether two JSON values are the same, whatever order their objects' keys
 * come in: the daemon's maps (an album's sort and filter values, say) don't
 * keep one.
 */
export function sameJson(a: unknown, b: unknown): boolean {
	if (a === b) return true;
	if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
	if (Array.isArray(a) || Array.isArray(b)) {
		if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
		return a.every((v, i) => sameJson(v, b[i]));
	}
	const x = a as Record<string, unknown>;
	const y = b as Record<string, unknown>;
	const keys = Object.keys(x);
	return (
		keys.length === Object.keys(y).length &&
		keys.every((k) => Object.hasOwn(y, k) && sameJson(x[k], y[k]))
	);
}
