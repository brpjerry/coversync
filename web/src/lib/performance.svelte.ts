import { readPref, writePref } from './browse.ts';

const KEY = 'coversync.performance';

/**
 * Performance mode: the costly drawing (blurs, frosted glass, the cover's
 * flight between grid and sheet) is left out, on this device only, since it
 * depends on the device's graphics, display and battery. Kept in the browser,
 * so in the desktop app it's per server (each has its own address there).
 * Styles key on `data-performance` on <html> (see app.css).
 */
class PerformanceMode {
	#on = $state(readPref(KEY, (v): v is 'on' | 'off' => v === 'on' || v === 'off', 'off') === 'on');

	constructor() {
		this.#apply();
	}

	get on() {
		return this.#on;
	}

	set on(on: boolean) {
		this.#on = on;
		writePref(KEY, on ? 'on' : 'off');
		this.#apply();
	}

	#apply() {
		document.documentElement.toggleAttribute('data-performance', this.#on);
	}
}

export const performanceMode = new PerformanceMode();
