// The desktop's notifications, for what finishes while the window is in the
// background. Only in the desktop app (see desktop.svelte.ts), which shows
// the page's notifications as the desktop's, lets the page send them without
// asking the user, and leaves out those sent while its window has the focus.
// In a browser nothing is sent, and nothing is asked.

import { desktopApp } from './desktop.svelte';

/**
 * Tell the user that something finished. Another with the same `tag` takes
 * this one's place. Clicking it brings the app's window up.
 */
export async function notify(title: string, body: string, tag: string) {
	if (!desktopApp.has('notify') || !('Notification' in window)) return;
	// The app answers at once: there is no prompt.
	if (Notification.permission === 'default') await Notification.requestPermission().catch(() => {});
	if (Notification.permission !== 'granted') return;
	const shown = new Notification(title, { body, tag });
	shown.onclick = () => shown.close();
}
