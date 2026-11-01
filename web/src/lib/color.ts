// Turn a cover's dominant colour into tones that read well on either theme.
// light-dark() picks by the element's color-scheme, which app.css sets per theme.

function hexToHsl(hex: string): [number, number, number] {
	const n = parseInt(hex.slice(1), 16);
	const r = ((n >> 16) & 255) / 255;
	const g = ((n >> 8) & 255) / 255;
	const b = (n & 255) / 255;
	const max = Math.max(r, g, b);
	const min = Math.min(r, g, b);
	const l = (max + min) / 2;
	if (max === min) return [0, 0, l * 100];
	const d = max - min;
	const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
	const h =
		max === r ? (g - b) / d + (g < b ? 6 : 0) : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
	return [h * 60, s * 100, l * 100];
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const FALLBACK = '#7c8aa5';

/**
 * A vivid, readable accent (buttons, progress, active rows): light on the
 * dark theme, deeper on the light one so white ink and tinted text still read.
 */
export function accent(hex: string | null): string {
	const [h, s, l] = hexToHsl(hex ?? FALLBACK);
	if (s < 8) return `light-dark(hsl(${h} 10% 42%), hsl(${h} 10% 72%))`;
	const hue = h.toFixed(0);
	const sat = clamp(s, 45, 85).toFixed(0);
	return `light-dark(hsl(${hue} ${sat}% ${clamp(l, 38, 46).toFixed(0)}%), hsl(${hue} ${sat}% ${clamp(l, 62, 72).toFixed(0)}%))`;
}

/** A deep tone for backgrounds and placeholders; a pale one on the light theme. */
export function shade(hex: string | null, lightness = 18): string {
	const [h, s] = hexToHsl(hex ?? FALLBACK);
	const hue = h.toFixed(0);
	return `light-dark(hsl(${hue} ${clamp(s, 0, 45).toFixed(0)}% 90%), hsl(${hue} ${clamp(s, 0, 55).toFixed(0)}% ${lightness}%))`;
}
