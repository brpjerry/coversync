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

/** sRGB channels, 0 to 1, of an HSL colour (h in degrees, s and l in percent). */
function hslToRgb(h: number, s: number, l: number): [number, number, number] {
	const sat = s / 100;
	const light = l / 100;
	const c = (1 - Math.abs(2 * light - 1)) * sat;
	const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
	const m = light - c / 2;
	const [r, g, b] =
		h < 60
			? [c, x, 0]
			: h < 120
				? [x, c, 0]
				: h < 180
					? [0, c, x]
					: h < 240
						? [0, x, c]
						: h < 300
							? [x, 0, c]
							: [c, 0, x];
	return [r + m, g + m, b + m];
}

/** WCAG relative luminance of the 8-bit colour the browser will actually paint. */
function luminance([r, g, b]: [number, number, number]): number {
	const lin = (v: number) => {
		const q = Math.round(v * 255) / 255;
		return q <= 0.04045 ? q / 12.92 : ((q + 0.055) / 1.055) ** 2.4;
	};
	return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** Contrast ratio of white ink on the colour. */
const whiteOn = (h: number, s: number, l: number) => 1.05 / (luminance(hslToRgb(h, s, l)) + 0.05);

/**
 * The lightest whole-number lightness at or below `start` that gives white
 * ink WCAG AA contrast (4.5:1). A fixed lightness isn't enough: at 46% a
 * yellow is still bright while a blue is already deep. `h` and `s` are the
 * rounded values that end up in the CSS, so the check sees the emitted colour.
 */
function readable(h: number, s: number, start: number): number {
	let l = Math.round(start);
	while (l > 18 && whiteOn(h, s, l) < 4.5) l -= 1;
	return l;
}

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const FALLBACK = '#7c8aa5';

/** Browsers without light-dark() keep the dark theme's tones rather than losing the colour. */
const LIGHT_DARK = typeof CSS !== 'undefined' && CSS.supports?.('color', 'light-dark(#000, #fff)');
const lightDark = (light: string, dark: string) =>
	LIGHT_DARK ? `light-dark(${light}, ${dark})` : dark;

/**
 * A vivid, readable accent (buttons, progress, active rows): light on the
 * dark theme, deeper on the light one so white ink and tinted text still read.
 */
export function accent(hex: string | null): string {
	const [h, s, l] = hexToHsl(hex ?? FALLBACK);
	if (s < 8) return lightDark(`hsl(${h} 10% 42%)`, `hsl(${h} 10% 72%)`);
	const hue = Math.round(h);
	const sat = Math.round(clamp(s, 45, 85));
	const deep = readable(hue, sat, clamp(l, 38, 46));
	return lightDark(
		`hsl(${hue} ${sat}% ${deep}%)`,
		`hsl(${hue} ${sat}% ${clamp(l, 62, 72).toFixed(0)}%)`
	);
}

/** A deep tone for backgrounds and placeholders; a pale one on the light theme. */
export function shade(hex: string | null, lightness = 18): string {
	const [h, s] = hexToHsl(hex ?? FALLBACK);
	const hue = h.toFixed(0);
	return lightDark(
		`hsl(${hue} ${clamp(s, 0, 45).toFixed(0)}% 90%)`,
		`hsl(${hue} ${clamp(s, 0, 55).toFixed(0)}% ${lightness}%)`
	);
}
