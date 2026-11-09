import { slide as svelteSlide, type SlideParams } from 'svelte/transition';
import { performanceMode } from './performance.svelte.ts';

/**
 * Svelte's slide, which animates the height and lays the page out again
 * every frame: in performance mode the element comes and goes at once.
 */
export function slide(node: Element, params?: SlideParams) {
	return svelteSlide(node, performanceMode.on ? { ...params, duration: 0 } : params);
}
