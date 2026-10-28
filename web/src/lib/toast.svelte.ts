export type ToastKind = 'info' | 'success' | 'error' | 'progress';

export interface ToastAction {
	label: string;
	run: () => void;
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
	toasts.push({ id, kind, message, action });
	if (ms > 0) setTimeout(() => dismiss(id), ms);
	return id;
}

export function dismiss(id: number) {
	const i = toasts.findIndex((t) => t.id === id);
	if (i >= 0) toasts.splice(i, 1);
}
