// The fallback page's way to the app (see bridge.rs). This runs in the script world "coversync",
// on the app's own pages only (coversync://), where the "coversync" message handler is: a
// server's pages, in the page's own world, see neither. It passes the page's requests (events
// from ui/bridge.js) to the app and the app's answers back.
(() => {
	if (location.protocol !== 'coversync:') return;
	const respond = (answer) =>
		document.dispatchEvent(
			new CustomEvent('coversync-response', { detail: JSON.stringify(answer) })
		);
	document.addEventListener('coversync-request', async (event) => {
		let id = null;
		try {
			const request = JSON.parse(event.detail);
			id = request.id;
			const answer = await window.webkit.messageHandlers.coversync.postMessage(
				JSON.stringify({ cmd: request.cmd, args: request.args ?? {} })
			);
			respond({ id, ...answer });
		} catch (err) {
			respond({
				id,
				ok: false,
				error: { title: 'Something went wrong', message: String(err?.message ?? err) }
			});
		}
	});
})();
