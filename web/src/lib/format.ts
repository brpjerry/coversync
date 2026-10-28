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
