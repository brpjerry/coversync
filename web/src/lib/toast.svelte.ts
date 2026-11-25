import { session } from './session.svelte';

export type ToastKind = 'info' | 'success' | 'error' | 'progress';

export interface ToastAction {
	label: string;
	run: () => void;
	/** Running it leaves the toast up, to run again (it goes with its ×). */
	keep?: boolean;
}

export interface Toast {
	id: number;
	kind: ToastKind;
	message: string;
	action?: ToastAction;
}

let next = 0;
export const toasts = $state<Toast[]>([]);

/** Show a toast; `ms = 0` keeps it until dismissed. Returns its id. */
export function toast(message: string, kind: ToastKind = 'info', ms = 4200, action?: ToastAction) {
	const id = next++;
	// A request turned away for want of a login: the login view says so.
	if (kind === 'error' && session.state === 'out') return id;
	toasts.push({ id, kind, message, action });
	if (ms > 0) setTimeout(() => dismiss(id), ms);
	return id;
}

export function dismiss(id: number) {
	const i = toasts.findIndex((t) => t.id === id);
	if (i >= 0) toasts.splice(i, 1);
}

/** Change what a toast still showing says. */
export function update(id: number, message: string) {
	const t = toasts.find((t) => t.id === id);
	if (t) t.message = message;
}
