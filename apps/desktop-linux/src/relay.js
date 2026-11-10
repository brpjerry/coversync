// The fallback page's way to the app (see bridge.rs). The window adds this script to every page,
// in a script world of its own; it does nothing except on the app's own pages (coversync://),
// where it passes the page's requests (events from ui/bridge.js) to the app and the app's answers
// back. The requests go as POSTs to the pages' own origin, which a server's pages can't reach,
// and the app checks where each one came from.
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
			const response = await fetch('coversync://app/__bridge', {
				method: 'POST',
				body: JSON.stringify({ cmd: request.cmd, args: request.args ?? {} })
			});
			respond({ id, ...(await response.json()) });
		} catch (err) {
			respond({
				id,
				ok: false,
				error: { title: 'Something went wrong', message: String(err?.message ?? err) }
			});
		}
	});
})();
