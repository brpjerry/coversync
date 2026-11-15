export function duration(ms: number): string {
	const s = Math.round(ms / 1000);
	const h = Math.floor(s / 3600);
	const m = Math.floor((s % 3600) / 60);
	const sec = String(s % 60).padStart(2, '0');
	return h ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
}

export function longDuration(ms: number): string {
	const min = Math.round(ms / 60000);
	return min >= 60 ? `${Math.floor(min / 60)} h ${min % 60} min` : `${min} min`;
}

export function quality(t: {
	format: string;
	sample_rate: number | null;
	bit_depth: number | null;
	bitrate: number | null;
}): string {
	const parts = [t.format];
	if (t.bit_depth && t.sample_rate)
		parts.push(`${t.bit_depth}/${+(t.sample_rate / 1000).toFixed(1)}`);
	else if (t.bitrate) parts.push(`${t.bitrate} kbps`);
	return parts.join(' · ');
}

const number = new Intl.NumberFormat();
export const count = (n: number) => number.format(n);

/** Lowercased, accent-stripped text for searching. */
export const fold = (s: string) => s.normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();

/** Bytes in decimal units, as disks and networks count them: "2.07 GB". */
export function bytes(n: number): string {
	if (n < 1000) return `${n} B`;
	const units = ['kB', 'MB', 'GB', 'TB'];
	let i = -1;
	do {
		n /= 1000;
		i++;
	} while (n >= 1000 && i < units.length - 1);
	return `${n >= 100 ? Math.round(n) : n.toFixed(n >= 10 ? 1 : 2)} ${units[i]}`;
}

/** Roughly how long is left: "8 s", "3 min", "1 h 5 min". */
export function timeLeft(seconds: number): string {
	if (seconds < 60) return `${Math.max(1, Math.round(seconds))} s`;
	if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
	return longDuration(seconds * 1000);
}
