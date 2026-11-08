<script lang="ts">
	import { fade, fly, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import FolderSync from '@lucide/svelte/icons/folder-sync';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import MonitorSmartphone from '@lucide/svelte/icons/monitor-smartphone';
	import Smartphone from '@lucide/svelte/icons/smartphone';
	import Laptop from '@lucide/svelte/icons/laptop';
	import Music from '@lucide/svelte/icons/music';
	import Plus from '@lucide/svelte/icons/plus';
	import Pencil from '@lucide/svelte/icons/pencil';
	import ShieldX from '@lucide/svelte/icons/shield-x';
	import Trash2 from '@lucide/svelte/icons/trash-2';
	import KeyRound from '@lucide/svelte/icons/key-round';
	import FileBadge from '@lucide/svelte/icons/file-badge';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import Lock from '@lucide/svelte/icons/lock';
	import { api, ApiError, type CaInfo, type Device, type Scope } from '#lib/api.ts';
	import { ago, devices, platformName, SCOPES } from '#lib/devices.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';
	import Toggle from './Toggle.svelte';
	import AddDevice from './AddDevice.svelte';

	interface Props {
		back: Back;
	}
	let { back }: Props = $props();

	syncSetup.refresh();
	devices.load();

	let adding = $state(false);
	/** The device being renamed or re-scoped, and its draft. */
	let editing = $state<number | null>(null);
	let draftName = $state('');
	let draftScope = $state<Scope>('manage');
	/** A revoke or remove waiting for its confirmation. */
	let confirming = $state<{ id: number; action: 'revoke' | 'remove' } | null>(null);

	function edit(d: Device) {
		confirming = null;
		editing = d.id;
		draftName = d.name;
		draftScope = d.scope;
	}

	async function saveEdit(d: Device) {
		const change: { name?: string; scope?: Scope } = {};
		if (draftName.trim() !== d.name) change.name = draftName;
		if (draftScope !== d.scope) change.scope = draftScope;
		if (!Object.keys(change).length || (await devices.update(d.id, change))) editing = null;
	}

	async function confirm(d: Device) {
		const action = confirming?.action;
		const done = action === 'remove' ? await devices.remove(d.id) : await devices.revoke(d.id);
		if (done) confirming = null;
	}

	function icon(d: Device) {
		const p = d.platform?.toLowerCase() ?? '';
		if (p === 'android' || p === 'ios') return Smartphone;
		if (['linux', 'windows', 'macos'].includes(p)) return Laptop;
		if (p === 'rockbox' || d.scope === 'sync') return Music;
		return MonitorSmartphone;
	}

	function details(d: Device) {
		return [
			d.platform && platformName(d.platform),
			d.app_version && `app ${d.app_version}`,
			d.hostname,
			`paired ${ago(d.paired_at)}`
		].filter(Boolean);
	}

	// The CA certificate path, checked as it's typed (after a short pause), and
	// again after signing back in.
	let caCheck = $state<{
		path: string;
		ca: CaInfo | null;
		error: string | null;
		/** When it was asked, to tell it from a refused save of the same path. */
		at: number;
	} | null>(null);
	let caChecking = $state(false);
	let seq = 0;
	$effect(() => {
		void session.generation;
		const path = syncSetup.caPath.trim();
		const mine = ++seq;
		if (!path || path === syncSetup.savedCaPath || !devices.canManage) {
			caChecking = false;
			return;
		}
		caChecking = true;
		const timer = setTimeout(async () => {
			let result: typeof caCheck;
			const at = Date.now();
			try {
				result = { path, ca: await api.checkCa(path), error: null, at };
			} catch (e) {
				result = {
					path,
					ca: null,
					error: e instanceof ApiError ? e.message : 'Could not check this file',
					at
				};
			}
			if (mine === seq) {
				caCheck = result;
				caChecking = false;
			}
		}, 300);
		return () => clearTimeout(timer);
	});

	type Verdict = { tone: 'ok' | 'none' | 'error'; text: string; sha?: string };
	let caVerdict = $derived.by((): Verdict | null => {
		const path = syncSetup.caPath.trim();
		if (!path)
			return {
				tone: 'none',
				text: "None. Apps trust the server's certificate as their system does."
			};
		if (path === syncSetup.savedCaPath) {
			if (syncSetup.ca) return { tone: 'ok', text: 'In use. SHA-256', sha: syncSetup.ca.sha256 };
			if (syncSetup.caProblem) return { tone: 'error', text: syncSetup.caProblem };
			return null;
		}
		// Why saving refused this path, until a later check of it says otherwise.
		const refused = syncSetup.caRefused?.path === path ? syncSetup.caRefused : null;
		if (caChecking || caCheck?.path !== path || (refused && refused.at > caCheck.at))
			return refused ? { tone: 'error', text: refused.why } : null;
		if (caCheck.error) return { tone: 'error', text: caCheck.error };
		if (caCheck.ca)
			return { tone: 'ok', text: 'A CA certificate. SHA-256', sha: caCheck.ca.sha256 };
		return null;
	});
</script>

<SetupPane {back} title="Sync setup" description="Copying the Library to your devices.">
	{#if syncSetup.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the sync setup from coversyncd.
			<button class="btn sm" onclick={() => syncSetup.load()}>Retry</button>
		</div>
	{:else if !syncSetup.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="sync-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><FolderSync size={20} /></span>
				<div>
					<h2 id="sync-title">Sync</h2>
					<p>
						Sync will keep devices such as phones and music players up to date with the Library,
						each with its own settings. Devices can pair now; copying files to them comes later.
					</p>
				</div>
			</header>
			<div class="card-body toggles">
				<Toggle
					bind:checked={syncSetup.enabled}
					label="Sync"
					description="When it's off, the sidebar's Sync section is hidden."
				/>
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="devices-title"
			in:fly={{ y: 12, duration: 420, delay: 60 }}
		>
			<header class="card-head with-action">
				<span class="card-icon"><MonitorSmartphone size={20} /></span>
				<div class="head-text">
					<h2 id="devices-title">Devices</h2>
					<p>
						Apps and players paired with this server. Each has its own token, which can be revoked
						without touching the others.
					</p>
				</div>
				{#if devices.canManage}
					<button class="btn primary add" onclick={() => (adding = true)}>
						<Plus size={16} /> Add device
					</button>
				{/if}
			</header>
			<div class="card-body devices">
				{#if devices.loaded && !devices.canManage}
					<p class="locked" role="note">
						<Lock size={15} /> Adding, changing and revoking devices needs the password: sign in to coversync
						in a browser with it. An app's own sign-in can't.
					</p>
				{/if}
				{#if devices.failed}
					<div class="notice" role="alert">
						<TriangleAlert size={18} /> Couldn't load the devices.
						<button class="btn sm" onclick={() => devices.load()}>Retry</button>
					</div>
				{:else if devices.loaded && !devices.list.length}
					<div class="empty">
						<p><b>No devices yet</b></p>
						<p>
							Add the desktop app, your phone or a music player: it gets a code to pair with, as a
							QR code or to type in.
						</p>
					</div>
				{:else}
					<ul class="list">
						{#each devices.list as d (d.id)}
							{@const Icon = icon(d)}
							{@const revoked = d.revoked_at !== null}
							<li class="device" class:revoked transition:slide={{ duration: 220 }}>
								<span class="dev-icon" aria-hidden="true"><Icon size={18} strokeWidth={1.8} /></span
								>
								{#if editing === d.id}
									<form
										class="edit"
										onsubmit={(e) => {
											e.preventDefault();
											saveEdit(d);
										}}
									>
										<input
											bind:value={draftName}
											aria-label="Name"
											maxlength="64"
											autocomplete="off"
											spellcheck="false"
										/>
										<div class="segmented" role="radiogroup" aria-label="What it may do">
											{#each Object.entries(SCOPES) as [id, s] (id)}
												<label class:on={draftScope === id} title={s.description}>
													<input type="radio" value={id} bind:group={draftScope} />
													{s.label}
												</label>
											{/each}
										</div>
										<div class="actions">
											<button type="button" class="btn sm" onclick={() => (editing = null)}>
												Cancel
											</button>
											<button
												class="btn sm primary"
												disabled={!draftName.trim() || devices.busy[d.id]}
											>
												Save
											</button>
										</div>
									</form>
								{:else}
									<div class="info">
										<p class="name">
											<b>{d.name}</b>
											<span class="tag">{SCOPES[d.scope].label}</span>
											{#if devices.thisDevice === d.id}<span class="tag this">This app</span>{/if}
											{#if revoked}<span class="tag bad">Revoked</span>{/if}
										</p>
										<p class="meta tabular">
											{#if revoked && d.revoked_at}
												Revoked {ago(d.revoked_at)}
											{:else if d.last_seen_at}
												Last seen {ago(d.last_seen_at)}{d.last_ip ? ` from ${d.last_ip}` : ''}
											{:else}
												Not used yet
											{/if}
										</p>
										<p class="meta">{details(d).join(' · ')}</p>
									</div>
									{#if devices.canManage}
										{#if confirming?.id === d.id}
											<div class="actions confirm" in:fade={{ duration: 150 }}>
												<span>
													{confirming.action === 'revoke'
														? 'Its token stops working at once.'
														: 'Forget it and its settings?'}
												</span>
												<button class="btn sm" onclick={() => (confirming = null)}>Cancel</button>
												<button
													class="btn sm danger"
													onclick={() => confirm(d)}
													disabled={devices.busy[d.id]}
												>
													{#if devices.busy[d.id]}<span class="spin"
															><LoaderCircle size={14} /></span
														>{/if}
													{confirming.action === 'revoke' ? 'Revoke' : 'Remove'}
												</button>
											</div>
										{:else}
											<div class="actions">
												{#if !revoked}
													<button class="btn sm" onclick={() => edit(d)}>
														<Pencil size={14} /> Edit
													</button>
													<button
														class="btn sm danger"
														onclick={() => (confirming = { id: d.id, action: 'revoke' })}
													>
														<ShieldX size={14} /> Revoke
													</button>
												{:else}
													<button
														class="btn sm"
														onclick={() => (confirming = { id: d.id, action: 'remove' })}
													>
														<Trash2 size={14} /> Remove
													</button>
												{/if}
											</div>
										{/if}
									{/if}
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="pairing-title"
			in:fly={{ y: 12, duration: 420, delay: 90 }}
		>
			<header class="card-head">
				<span class="card-icon"><KeyRound size={20} /></span>
				<div>
					<h2 id="pairing-title">Pairing</h2>
					<p>How devices reach this server safely when they pair.</p>
				</div>
			</header>
			<div class="card-body pairing">
				<div class="ca">
					<label for="ca-path" class="field-label">
						<FileBadge size={15} /> CA certificate for apps
					</label>
					<input
						id="ca-path"
						class="path"
						class:bad={caVerdict?.tone === 'error'}
						bind:value={syncSetup.caPath}
						placeholder="~/.local/share/mkcert/rootCA.pem"
						disabled={!devices.canManage}
						spellcheck="false"
						autocomplete="off"
						autocapitalize="off"
					/>
					{#if caChecking}
						<p class="verdict muted">
							<span class="spin"><LoaderCircle size={14} /></span> Checking…
						</p>
					{:else if caVerdict}
						<p class="verdict {caVerdict.tone}" in:fade={{ duration: 160 }}>
							{#if caVerdict.tone === 'ok'}<CircleCheck size={14} />
							{:else if caVerdict.tone === 'error'}<CircleX size={14} />
							{:else}<CircleAlert size={14} />{/if}
							<span>
								{caVerdict.text}
								{#if caVerdict.sha}<code class="sha">{caVerdict.sha}</code>{/if}
							</span>
						</p>
					{/if}
					<p class="help">
						For a server whose certificate a CA of your own signed, such as mkcert's: the path to
						that CA's public certificate, never its key. The Android app downloads it, checks it
						against the QR code, and trusts it for this server alone. Leave it empty when the
						server's certificate is publicly trusted. Desktop apps use the system's certificates.
					</p>
				</div>
				<Toggle
					bind:checked={syncSetup.allowHttp}
					label="Allow pairing over plain http"
					description="Only for a network that is encrypted already, such as Tailscale or WireGuard. Pairing through coversync's https:// address, or on this computer, works without it."
					disabled={!devices.canManage}
				/>
				{#if syncSetup.allowHttp}
					<div class="warn" role="note" transition:slide={{ duration: 220, easing: cubicOut }}>
						<TriangleAlert size={17} />
						<p>
							Over plain http, pairing codes and tokens cross the network unencrypted: anyone who
							can see the traffic can take them, and with them the Library.
						</p>
					</div>
				{/if}
			</div>
		</section>

		<SaveBar
			changes={syncSetup.changes}
			saving={syncSetup.saving}
			saveLabel="Save"
			onrevert={() => syncSetup.revert()}
			onsave={() => syncSetup.save()}
		/>
	{/if}
</SetupPane>

{#if adding}
	<AddDevice onclose={() => (adding = false)} />
{/if}

<style>
	.toggles {
		padding-top: 12px;
	}
	/* The button wraps under the text when there's no room beside it. */
	.with-action {
		flex-wrap: wrap;
	}
	.head-text {
		flex: 1 1 260px;
	}
	.add {
		flex: none;
		margin-left: auto;
		align-self: center;
	}
	.devices {
		display: grid;
		gap: 14px;
		padding-top: 8px;
	}
	.locked {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		margin: 8px 0 0;
		font-size: 13.5px;
		color: var(--text-2);
	}
	.locked :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	.empty {
		padding: 18px 4px 6px;
		color: var(--text-2);
		font-size: 14px;
	}
	.empty p {
		margin: 0 0 4px;
	}
	.empty b {
		color: var(--text);
		font-weight: 600;
	}
	.list {
		display: grid;
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.device {
		display: grid;
		grid-template-columns: 36px minmax(0, 1fr) auto;
		align-items: center;
		gap: 14px;
		padding: 14px 0;
		border-bottom: 1px solid var(--border);
	}
	.device:last-child {
		border-bottom: 0;
	}
	.dev-icon {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: 11px;
		background: var(--surface-hover);
		color: var(--text-2);
	}
	.revoked .dev-icon,
	.revoked .info {
		opacity: 0.55;
	}
	.info p {
		margin: 0;
	}
	.name {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
	}
	.name b {
		font-weight: 600;
		font-size: 15px;
	}
	.tag {
		padding: 1px 8px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-2);
		background: var(--surface-hover);
	}
	.tag.this {
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}
	.tag.bad {
		color: var(--danger);
		background: color-mix(in oklab, var(--danger) 14%, transparent);
	}
	.meta {
		font-size: 13px;
		color: var(--text-3);
		overflow-wrap: anywhere;
	}
	.meta:first-of-type {
		color: var(--text-2);
	}
	.actions {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		flex-wrap: wrap;
		gap: 8px;
	}
	.confirm span {
		font-size: 13px;
		color: var(--text-2);
	}
	.danger {
		color: var(--danger);
		border-color: color-mix(in oklab, var(--danger) 40%, transparent);
	}
	.edit {
		grid-column: 2 / -1;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
	}
	.edit input:not([type='radio']) {
		flex: 1 1 200px;
		height: 36px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--border-strong);
		background: var(--bg);
		color: var(--text);
		font: inherit;
		font-size: 14px;
		outline: 0;
	}
	.edit input:not([type='radio']):focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.segmented {
		display: inline-flex;
		padding: 3px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.segmented label {
		padding: 5px 12px;
		border-radius: 999px;
		font-size: 13px;
		font-weight: 560;
		color: var(--text-2);
		cursor: pointer;
		transition:
			background 160ms,
			color 160ms;
	}
	.segmented label.on {
		color: var(--accent-ink);
		background: var(--accent);
	}
	.segmented label:has(:focus-visible) {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.segmented input {
		position: absolute;
		opacity: 0;
		pointer-events: none;
	}

	.pairing {
		display: grid;
		gap: 16px;
	}
	.ca {
		display: grid;
		gap: 8px;
	}
	.field-label {
		display: flex;
		align-items: center;
		gap: 7px;
		font-size: 13px;
		font-weight: 600;
		color: var(--text-2);
	}
	.path {
		height: 44px;
		padding: 0 14px;
		border-radius: 12px;
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 14px;
		outline: 0;
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.path:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.path.bad {
		border-color: color-mix(in oklab, var(--danger) 60%, transparent);
	}
	.path:disabled {
		opacity: 0.6;
	}
	.verdict {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		margin: 0;
		font-size: 13px;
	}
	.verdict :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	.verdict.ok {
		color: var(--success);
	}
	.verdict.none,
	.muted {
		color: var(--text-3);
	}
	.verdict.error {
		color: var(--danger);
	}
	.sha {
		display: block;
		margin-top: 2px;
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 12px;
		color: var(--text-2);
		overflow-wrap: anywhere;
	}
	.help {
		margin: 0;
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
	.spin {
		display: inline-grid;
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}

	@media (max-width: 760px) {
		.device {
			grid-template-columns: 36px minmax(0, 1fr);
		}
		.device > .actions {
			grid-column: 2;
			justify-content: flex-start;
		}
	}
</style>
