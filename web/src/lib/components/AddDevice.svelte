<script lang="ts">
	import { onDestroy, onMount, tick } from 'svelte';
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import X from '@lucide/svelte/icons/x';
	import Smartphone from '@lucide/svelte/icons/smartphone';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Copy from '@lucide/svelte/icons/copy';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import ShieldX from '@lucide/svelte/icons/shield-x';
	import {
		api,
		ApiError,
		BASE,
		type Device,
		type NewPairing,
		type PairingStatus,
		type Scope
	} from '#lib/api.ts';
	import { devices, platformName, SCOPES } from '#lib/devices.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import { toast } from '#lib/toast.svelte.ts';
	import QrCode from './QrCode.svelte';

	interface Props {
		onclose: () => void;
	}
	let { onclose }: Props = $props();

	/** How often the dialog asks whether a device has used the code. */
	const POLL_MS = 1500;

	let name = $state('');
	let scope = $state<Scope>('manage');
	let replacing = $state(false);
	let replace = $state<number | null>(null);

	let phase = $state<'form' | 'code' | 'paired' | 'ended'>('form');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let pairing = $state<NewPairing | null>(null);
	let status = $state<PairingStatus | null>(null);
	/** When the code expires, by this browser's clock. */
	let deadline = $state(0);
	let now = $state(Date.now());
	let revoked = $state(false);
	/** Copying the link failed: it's shown to copy by hand. */
	let showLink = $state(false);

	/** The address devices reach coversync at: this page's own, with its path prefix. */
	const server = `${location.origin}${BASE}/`;

	/** Whether a host name is this machine, as URLs write it (`[::ffff:7f00:1]` for ::ffff:127.0.0.1). */
	function isLoopback(host: string) {
		const h = host.toLowerCase().replace(/\.$/, '');
		return (
			h === 'localhost' ||
			h.endsWith('.localhost') ||
			/^127(\.\d{1,3}){3}$/.test(h) ||
			h === '[::1]' ||
			/^\[::ffff:7f[0-9a-f]{2}:[0-9a-f]{1,4}\]$/.test(h)
		);
	}
	/** Only apps on this computer can reach coversync at this page's address. */
	const loopback = isLoopback(location.hostname);
	/** Devices given this page's address would pair over plain http, which the server refuses. */
	let insecure = $derived(
		location.protocol === 'http:' && !loopback && !(pairing?.allow_http ?? syncSetup.savedAllowHttp)
	);
	let link = $derived(
		pairing
			? `coversync://pair?server=${encodeURIComponent(server)}&code=${pairing.code}` +
					(pairing.ca ? `&ca=${pairing.ca}` : '')
			: ''
	);
	let shown = $derived(pairing ? `${pairing.code.slice(0, 4)}-${pairing.code.slice(4)}` : '');
	let left = $derived(Math.max(0, Math.ceil((deadline - now) / 1000)));
	let paired = $derived(status?.state === 'paired' ? status.device : null);
	let candidates = $derived(devices.list);

	function pick(id: number | null) {
		replace = id;
		const d = devices.list.find((x) => x.id === id);
		// Keeps what it had, unless changed here.
		if (d) {
			name = d.name;
			scope = d.scope;
		}
	}

	let timer: ReturnType<typeof setTimeout> | undefined;
	let ticker: ReturnType<typeof setInterval> | undefined;
	/** The code on the server this dialog holds, until it closes it. */
	let held: string | null = null;
	/** Bumped by forget(): a code made for an earlier attempt is closed when it arrives. */
	let attempt = 0;

	function stop() {
		clearTimeout(timer);
		clearInterval(ticker);
	}

	async function start() {
		// One code at a time, however often its buttons are clicked.
		if (busy || !name.trim() || (replacing && replace === null)) return;
		busy = true;
		error = null;
		forget();
		const mine = attempt;
		try {
			const made = await api.newPairing(name, scope, replacing ? replace : null);
			if (mine !== attempt) {
				// The dialog closed meanwhile: nobody would see this code.
				api.closePairing(made.id, true).catch(() => {});
				return;
			}
			pairing = made;
			held = made.id;
			status = { state: 'waiting', expires_in: made.expires_in };
			deadline = Date.now() + made.expires_in * 1000;
			now = Date.now();
			revoked = false;
			showLink = false;
			phase = 'code';
			ticker = setInterval(() => (now = Date.now()), 1000);
			timer = setTimeout(poll, POLL_MS);
		} catch (e) {
			if (mine === attempt)
				error = e instanceof ApiError ? e.message : 'Could not reach coversyncd.';
			// The device to replace is gone: the list says so too, and the form
			// comes back without it, to pair as a new device or to choose
			// another one to replace (if any is left).
			if (e instanceof ApiError && e.code === 'no_device') {
				devices.load();
				if (mine === attempt) {
					replace = null;
					replacing = false;
					phase = 'form';
				}
			}
		} finally {
			busy = false;
		}
	}

	async function poll() {
		const mine = pairing;
		if (!mine || held !== mine.id) return;
		// Signed out meanwhile: wait for the login view.
		if (session.state === 'out') {
			timer = setTimeout(poll, POLL_MS);
			return;
		}
		try {
			const s = await api.pairing(mine.id);
			// Closed, or a new code made, while this was asked.
			if (pairing !== mine || held !== mine.id) return;
			status = s;
			if (s.state === 'waiting') {
				// Keep the countdown to the server's clock.
				deadline = Date.now() + s.expires_in * 1000;
			} else if (s.state !== 'pairing') {
				stop();
				phase = s.state === 'paired' ? 'paired' : 'ended';
				// The list changed: a device paired, or the one to replace went.
				if (s.state === 'paired' || (s.state === 'cancelled' && s.why === 'removed'))
					devices.load();
				return;
			}
		} catch (e) {
			if (pairing !== mine || held !== mine.id) return;
			if (e instanceof ApiError && e.status === 404) {
				stop();
				status = { state: 'expired' };
				phase = 'ended';
				return;
			}
		}
		timer = setTimeout(poll, POLL_MS);
	}

	/**
	 * The code stops working, if unused, and nothing more is asked about it:
	 * the dialog is closing, or making a new one. What it shows stays until then.
	 */
	function forget(keepalive = false) {
		attempt++;
		stop();
		if (held) api.closePairing(held, keepalive).catch(() => {});
		held = null;
	}

	function close() {
		forget();
		onclose();
	}

	async function copyLink() {
		try {
			await navigator.clipboard.writeText(link);
			toast('Pairing link copied', 'success');
		} catch {
			// No clipboard here (a plain http page, say): show the link to copy by hand.
			showLink = true;
			await tick();
			const text = dialog?.querySelector('.link-text');
			if (text) getSelection()?.selectAllChildren(text);
		}
	}

	async function revokePaired() {
		if (paired && (await devices.revoke(paired.id))) revoked = true;
	}

	let dialog = $state<HTMLDivElement>();
	let closeButton = $state<HTMLButtonElement>();
	/** What had focus before the dialog opened (Add device), to give it back. */
	const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;

	/** The dialog's controls that take focus, in order. */
	function controls() {
		if (!dialog) return [];
		const all = dialog.querySelectorAll<HTMLElement>(
			'button, input, select, textarea, a[href], [tabindex]:not([tabindex="-1"])'
		);
		return [...all].filter(
			(el) => !(el as HTMLButtonElement).disabled && el.getClientRects().length > 0
		);
	}

	/** Keep Tab inside the dialog: past its last control back to its first, and the other way. */
	function trap(e: KeyboardEvent) {
		const list = controls();
		if (!list.length) return;
		const at = list.indexOf(document.activeElement as HTMLElement);
		if (e.shiftKey ? at <= 0 : at === list.length - 1 || at === -1) {
			e.preventDefault();
			(e.shiftKey ? list[list.length - 1] : list[0]).focus();
		}
	}

	function onkeydown(e: KeyboardEvent) {
		// The page's own keys (Escape leaves Sync setup) stay out of it.
		e.stopPropagation();
		if (e.key === 'Escape') close();
		else if (e.key === 'Tab') trap(e);
	}

	/** Keys pressed with focus outside the dialog belong to it too, but the login view's. */
	function onwindowkeydown(e: KeyboardEvent) {
		if (session.state === 'out' || dialog?.contains(e.target as Node)) return;
		e.stopPropagation();
		if (e.key === 'Escape') close();
		else if (e.key === 'Tab') trap(e);
	}

	// Each step moves focus to a control it has: the one it leaves is gone.
	$effect(() => {
		void phase;
		void revoked;
		tick().then(() =>
			(dialog?.querySelector<HTMLElement>('[data-focus]') ?? closeButton)?.focus({
				preventScroll: true
			})
		);
	});

	// Leaving the page closes the dialog, so a page restored from the
	// back-forward cache doesn't show a code that no longer works.
	function onpagehide() {
		forget(true);
		onclose();
	}
	onMount(() => {
		document.documentElement.style.overflow = 'hidden';
	});
	onDestroy(() => {
		document.documentElement.style.overflow = '';
		forget();
		if (opener?.isConnected) opener.focus({ preventScroll: true });
	});

	const ended: Record<string, string> = {
		expired: 'This code has expired. Codes work for 10 minutes.',
		failures:
			'This code was cancelled: too many wrong codes were tried, so every waiting code was.',
		insecure:
			'This code was cancelled: a device sent it over plain http, where it could have been seen on the way.',
		removed: 'The device this would replace was removed, so its code was cancelled.',
		failed: "The device's token couldn't be issued; coversyncd's log says why."
	};
	let endedWhy = $derived(
		status?.state === 'cancelled' ? ended[status.why] : ended[status?.state ?? 'expired']
	);
	const clock = (s: number) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
	const describe = (d: Device) =>
		[
			d.platform && platformName(d.platform),
			d.app_version && `app ${d.app_version}`,
			d.hostname,
			d.last_ip
		].filter(Boolean);
</script>

<svelte:window {onpagehide} onkeydowncapture={onwindowkeydown} />

<div
	class="scrim"
	role="presentation"
	onclick={(e) => e.target === e.currentTarget && close()}
	{onkeydown}
	transition:fade={{ duration: 200 }}
>
	<div
		bind:this={dialog}
		class="dialog"
		role="dialog"
		aria-modal="true"
		aria-labelledby="add-device-title"
		transition:fly={{ y: 24, duration: 360, easing: cubicOut }}
	>
		<header class="head">
			<span class="badge"><Smartphone size={20} /></span>
			<div>
				<h2 id="add-device-title">Add device</h2>
				<p>A phone, a computer or a music player, with a token of its own.</p>
			</div>
			<button bind:this={closeButton} class="icon-btn close" onclick={close} aria-label="Close"
				><X size={19} /></button
			>
		</header>

		{#if phase === 'form'}
			<form
				class="body form"
				onsubmit={(e) => {
					e.preventDefault();
					start();
				}}
			>
				<label class="field">
					<span class="label">Name</span>
					<input
						data-focus
						bind:value={name}
						placeholder="Pixel 8"
						maxlength="64"
						autocomplete="off"
						spellcheck="false"
					/>
				</label>

				<fieldset class="scopes">
					<legend class="label">What it may do</legend>
					{#each Object.entries(SCOPES) as [id, s] (id)}
						<label class="scope" class:on={scope === id}>
							<input type="radio" name="scope" value={id} bind:group={scope} />
							<span class="scope-title">{s.label}</span>
							<span class="scope-desc">{s.description}</span>
						</label>
					{/each}
				</fieldset>

				{#if candidates.length}
					<div class="replace">
						<label class="check">
							<input
								type="checkbox"
								bind:checked={replacing}
								onchange={() => !replacing && (replace = null)}
							/>
							Replace an existing device
						</label>
						<p class="hint">
							After reinstalling the app or rebuilding a computer: the device keeps its place and
							settings, and its old token stops working.
						</p>
						{#if replacing}
							<select
								aria-label="Device to replace"
								value={replace ?? ''}
								onchange={(e) => pick(Number(e.currentTarget.value) || null)}
								in:fade={{ duration: 150 }}
							>
								<option value="" disabled>Choose a device</option>
								{#each candidates as d (d.id)}
									<option value={d.id}>{d.name}{d.revoked_at ? ' (revoked)' : ''}</option>
								{/each}
							</select>
						{/if}
					</div>
				{/if}

				{#if insecure}
					<div class="warn" role="note">
						<TriangleAlert size={17} />
						<p>
							This page is on plain <code>http://</code>, so a device given its address would be
							refused: its token would cross the network unencrypted. Open coversync at its
							<code>https://</code> address, or allow plain http in Sync setup's Pairing for a network
							that is encrypted already.
						</p>
					</div>
				{/if}
				{#if error}<p class="error" role="alert"><CircleAlert size={15} /> {error}</p>{/if}

				<footer class="foot">
					<button type="button" class="btn" onclick={close}>Cancel</button>
					<button
						class="btn primary"
						disabled={busy || !name.trim() || (replacing && replace === null)}
					>
						{#if busy}<span class="spin"><LoaderCircle size={15} /></span>{/if}
						Show code
					</button>
				</footer>
			</form>
		{:else if phase === 'code' && pairing}
			<div class="body code-view" in:fade={{ duration: 200 }}>
				<div class="qr-box" data-link={link}>
					<QrCode text={link} label="Pairing QR code for {name}" />
				</div>
				<div class="code-side">
					<p class="label">Pairing code</p>
					<p class="code tabular" aria-label="Pairing code {shown.split('').join(' ')}">
						{shown}
					</p>
					<p class="expires tabular" class:soon={left < 60 && status?.state !== 'pairing'}>
						{#if status?.state === 'pairing'}
							Used just now
						{:else}
							{left > 0 ? `Expires in ${clock(left)}` : 'Expiring…'}
						{/if}
					</p>
					<ul class="steps">
						<li><b>Phone:</b> scan the QR code.</li>
						<li>
							<b>Computer or terminal:</b> enter the server address and the code.
						</li>
					</ul>
					<div class="server">
						<span class="label">Server address</span>
						<code>{server}</code>
					</div>
					<p class="waiting">
						<span class="spin"><LoaderCircle size={14} /></span>
						{status?.state === 'pairing' ? 'Pairing…' : `Waiting for ${name} to pair…`}
					</p>
				</div>
				{#if pairing.ca_problem}
					<div class="warn wide" role="note">
						<TriangleAlert size={17} />
						<p>
							The CA certificate for apps can't be used, so the QR code leaves it out: {pairing.ca_problem}
						</p>
					</div>
				{/if}
				{#if insecure}
					<div class="warn wide" role="note">
						<TriangleAlert size={17} />
						<p>
							Devices given this <code>http://</code> address will be refused. Use coversync's
							<code>https://</code> address.
						</p>
					</div>
				{:else if loopback}
					<div class="warn wide" role="note">
						<TriangleAlert size={17} />
						<p>
							This address only works on this computer: a phone or another computer can't reach it.
							To pair one of those, open coversync at the address they use, such as its
							<code>https://</code> one. Apps on this computer can use it only if it reaches
							coversyncd directly, not through a proxy over plain <code>http://</code>.
						</p>
					</div>
				{/if}
				{#if showLink}
					<div class="link-box wide" in:fade={{ duration: 150 }}>
						<span class="label">Pairing link (copy it from here)</span>
						<code class="link-text">{link}</code>
					</div>
				{/if}
			</div>
			<footer class="foot">
				<button class="btn sm link-btn" onclick={copyLink} data-focus
					><Copy size={14} /> Copy link</button
				>
				<button class="btn" onclick={close}>Cancel</button>
			</footer>
		{:else if phase === 'paired'}
			<div class="body result" in:fade={{ duration: 200 }}>
				{#if revoked}
					<span class="result-icon bad"><ShieldX size={34} strokeWidth={1.6} /></span>
					<h3>Revoked</h3>
					<p>That device's token no longer works. Make a new code for the right one.</p>
				{:else}
					<span class="result-icon"><CircleCheck size={34} strokeWidth={1.6} /></span>
					<h3>{paired?.name ?? name} is paired</h3>
					{#if paired}
						<p class="details">{describe(paired).join(' · ')}</p>
						<p class="not-you">
							Not you?
							<button
								class="btn sm danger"
								onclick={revokePaired}
								disabled={devices.busy[paired.id]}
							>
								Revoke
							</button>
						</p>
					{/if}
				{/if}
				{#if error}<p class="error" role="alert"><CircleAlert size={15} /> {error}</p>{/if}
			</div>
			<footer class="foot">
				{#if revoked}
					<button class="btn new-code" onclick={start} disabled={busy} data-focus>
						{#if busy}<span class="spin"><LoaderCircle size={15} /></span>{:else}<RefreshCw
								size={15}
							/>{/if} New code
					</button>
				{/if}
				<button class="btn primary" onclick={close} data-focus={revoked ? undefined : true}
					>Done</button
				>
			</footer>
		{:else}
			<div class="body result" in:fade={{ duration: 200 }}>
				<span class="result-icon bad"><CircleAlert size={34} strokeWidth={1.6} /></span>
				<h3>No device paired</h3>
				<p>{endedWhy}</p>
				{#if error}<p class="error" role="alert"><CircleAlert size={15} /> {error}</p>{/if}
			</div>
			<footer class="foot">
				<button class="btn" onclick={close}>Close</button>
				<button class="btn primary new-code" onclick={start} disabled={busy} data-focus>
					{#if busy}<span class="spin"><LoaderCircle size={15} /></span>{:else}<RefreshCw
							size={15}
						/>{/if} New code
				</button>
			</footer>
		{/if}
	</div>
</div>

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 45;
		display: grid;
		place-items: center;
		padding: 24px;
		background: rgb(0 0 0 / 0.5);
		backdrop-filter: blur(10px);
	}
	.dialog {
		width: min(620px, 100%);
		max-height: calc(100dvh - 48px);
		display: grid;
		grid-template-rows: auto minmax(0, 1fr) auto;
		border-radius: 24px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow: 0 40px 120px -30px rgb(0 0 0 / 0.75);
		overflow: hidden;
	}
	.head {
		display: flex;
		align-items: flex-start;
		gap: 14px;
		padding: 20px 22px 16px;
		border-bottom: 1px solid var(--border);
	}
	.badge {
		display: grid;
		place-items: center;
		flex: none;
		width: 40px;
		height: 40px;
		border-radius: 12px;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}
	.head div {
		flex: 1;
		min-width: 0;
	}
	h2 {
		margin: 0;
		font-size: 19px;
		letter-spacing: -0.01em;
	}
	.head p {
		margin: 2px 0 0;
		font-size: 13.5px;
		color: var(--text-2);
	}
	.close {
		margin: -4px -6px 0 0;
	}
	.body {
		padding: 20px 22px;
		overflow: auto;
	}
	.form {
		display: grid;
		gap: 18px;
		padding-bottom: 0;
	}
	.label {
		display: block;
		margin: 0 0 8px;
		padding: 0;
		font-size: 12.5px;
		font-weight: 550;
		color: var(--text-2);
	}
	.field input,
	select {
		width: 100%;
		height: 44px;
		padding: 0 14px;
		border-radius: 12px;
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text);
		font: inherit;
		font-size: 14.5px;
		outline: 0;
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.field input:focus,
	select:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	option {
		background: var(--bg-raised);
		color: var(--text);
	}
	.scopes {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 10px;
		margin: 0;
		padding: 0;
		border: 0;
	}
	.scopes legend {
		grid-column: 1 / -1;
	}
	.scope {
		position: relative;
		display: grid;
		gap: 4px;
		padding: 13px 40px 14px 15px;
		border-radius: 14px;
		border: 1px solid var(--border);
		background: var(--surface);
		cursor: pointer;
		transition:
			border-color 200ms,
			background 200ms,
			box-shadow 260ms;
	}
	.scope.on {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 12%, transparent);
	}
	.scope input {
		position: absolute;
		top: 14px;
		right: 14px;
		appearance: none;
		margin: 0;
		width: 17px;
		height: 17px;
		border-radius: 999px;
		border: 1.5px solid var(--border-strong);
		background: var(--bg);
		cursor: pointer;
	}
	.scope input:checked {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 4px var(--bg-raised);
		background: var(--accent);
	}
	.scope-title {
		font-weight: 620;
		font-size: 14.5px;
	}
	.scope-desc {
		font-size: 13px;
		color: var(--text-2);
	}
	.replace {
		display: grid;
		gap: 8px;
	}
	.check {
		display: flex;
		align-items: center;
		gap: 9px;
		font-weight: 560;
		font-size: 14px;
		cursor: pointer;
	}
	.check input {
		width: 16px;
		height: 16px;
		margin: 0;
		accent-color: var(--accent);
	}
	.hint {
		margin: -4px 0 0 25px;
		font-size: 13px;
		color: var(--text-3);
	}
	.warn {
		display: flex;
		gap: 10px;
		padding: 12px 14px;
		border-radius: 14px;
		background: var(--warn-surface);
		border: 1px solid var(--warn-border);
		color: var(--warn-text);
	}
	.warn :global(svg) {
		flex: none;
		margin-top: 2px;
		color: var(--warn);
	}
	.warn p {
		margin: 0;
		font-size: 13.5px;
	}
	.wide {
		grid-column: 1 / -1;
	}
	code {
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 0.92em;
	}
	.error {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
		font-size: 13.5px;
		color: var(--danger);
	}
	.foot {
		display: flex;
		justify-content: flex-end;
		align-items: center;
		gap: 10px;
		padding: 16px 22px 20px;
	}
	.form .foot {
		margin: 0 -22px;
	}
	.link-btn {
		height: 32px;
		margin-right: auto;
		padding: 0 12px;
		font-size: 13px;
	}

	.code-view {
		display: grid;
		grid-template-columns: 220px minmax(0, 1fr);
		gap: 18px 24px;
		align-items: start;
	}
	.qr-box {
		padding: 6px;
		border-radius: 18px;
		background: #fff;
		box-shadow: 0 10px 30px -14px rgb(0 0 0 / 0.6);
	}
	.code {
		margin: 0;
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 34px;
		font-weight: 650;
		letter-spacing: 0.08em;
		color: var(--text);
		user-select: all;
	}
	.expires {
		margin: 2px 0 14px;
		font-size: 13px;
		color: var(--text-3);
	}
	.expires.soon {
		color: var(--warn);
	}
	.steps {
		display: grid;
		gap: 4px;
		margin: 0 0 14px;
		padding: 0;
		list-style: none;
		font-size: 13.5px;
		color: var(--text-2);
	}
	.steps b {
		font-weight: 600;
		color: var(--text);
	}
	.server {
		margin-bottom: 14px;
	}
	.server .label {
		margin-bottom: 4px;
	}
	.server code {
		display: block;
		padding: 8px 10px;
		border-radius: 10px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-size: 13px;
		overflow-wrap: anywhere;
		user-select: all;
	}
	.waiting {
		display: flex;
		align-items: center;
		gap: 7px;
		margin: 0;
		font-size: 13px;
		color: var(--text-3);
	}

	.result {
		display: grid;
		justify-items: center;
		gap: 6px;
		padding: 28px 30px 8px;
		text-align: center;
	}
	.result-icon {
		color: var(--success);
	}
	.result-icon.bad {
		color: var(--danger);
	}
	h3 {
		margin: 6px 0 0;
		font-size: 19px;
		letter-spacing: -0.01em;
	}
	.result p {
		margin: 0;
		color: var(--text-2);
		font-size: 14px;
		max-width: 44ch;
	}
	.details {
		font-variant-numeric: tabular-nums;
	}
	.result .error {
		margin-top: 10px;
		color: var(--danger);
	}
	.link-box {
		display: grid;
		gap: 4px;
	}
	.link-text {
		display: block;
		padding: 8px 10px;
		border-radius: 10px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-size: 12.5px;
		overflow-wrap: anywhere;
		user-select: all;
	}
	.not-you {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-top: 10px !important;
	}
	.btn.sm {
		height: 32px;
		padding: 0 12px;
		font-size: 13px;
	}
	.danger {
		color: var(--danger);
		border-color: color-mix(in oklab, var(--danger) 40%, transparent);
	}
	.spin {
		display: inline-grid;
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}

	@media (max-width: 640px) {
		.scrim {
			padding: 0;
		}
		.dialog {
			max-height: 100dvh;
			height: 100dvh;
			border-radius: 0;
		}
		.scopes,
		.code-view {
			grid-template-columns: 1fr;
		}
		.qr-box {
			width: min(260px, 100%);
			justify-self: center;
		}
	}
</style>
