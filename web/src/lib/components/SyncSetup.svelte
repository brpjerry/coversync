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
	import SlidersHorizontal from '@lucide/svelte/icons/sliders-horizontal';
	import FolderTree from '@lucide/svelte/icons/folder-tree';
	import ImageIcon from '@lucide/svelte/icons/image';
	import {
		api,
		ApiError,
		type CaInfo,
		type Device,
		type DeviceSettings,
		type Scope
	} from '#lib/api.ts';
	import { ago, devices, platformName, SCOPES } from '#lib/devices.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import { setup } from '#lib/setup.svelte.ts';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';
	import Toggle from './Toggle.svelte';
	import AddDevice from './AddDevice.svelte';
	import StructureField from './StructureField.svelte';

	interface Props {
		back: Back;
	}
	let { back }: Props = $props();

	syncSetup.refresh();
	devices.load();
	// The Library's folder structure, and the variables a device's own can use.
	setup.load();

	let adding = $state(false);
	/** The device being renamed or re-scoped, and its draft. */
	let editing = $state<number | null>(null);
	let draftName = $state('');
	let draftScope = $state<Scope>('manage');
	/** A revoke or remove waiting for its confirmation. */
	let confirming = $state<{ id: number; action: 'revoke' | 'remove' } | null>(null);

	/** Bumped each time an editor opens, the same device's again included. */
	let opened = 0;

	function edit(d: Device) {
		opened++;
		confirming = null;
		syncing = null;
		editing = d.id;
		draftName = d.name;
		draftScope = d.scope;
	}

	async function saveEdit(d: Device) {
		const mine = opened;
		const [name, scope] = [draftName, draftScope];
		const change: { name?: string; scope?: Scope } = {};
		if (name.trim() !== d.name) change.name = name;
		if (scope !== d.scope) change.scope = scope;
		const saved = !Object.keys(change).length || (await devices.update(d.id, change));
		// Only the editor that was saved closes, and only as it was saved: one
		// opened since, or edited further meanwhile, stays.
		const same = mine === opened && editing === d.id;
		if (saved && same && draftName === name && draftScope === scope) editing = null;
	}

	/** The device whose sync settings are open, and their draft. */
	let syncing = $state<number | null>(null);
	let syncDraft = $state({
		own: false,
		structure: '',
		fatSafe: false,
		coverFile: true,
		coverName: 'cover',
		maxWidth: null as number | null
	});

	function editSync(d: Device) {
		if (!d.sync) return;
		opened++;
		confirming = null;
		editing = null;
		syncing = d.id;
		const s = d.sync.settings;
		syncDraft = {
			own: s.structure !== null,
			// Its own starts from the Library's.
			structure: s.structure ?? setup.saved.structure,
			fatSafe: s.fat_safe,
			coverFile: s.art.cover_file,
			coverName: s.art.cover_name,
			maxWidth: s.art.max_width
		};
	}

	async function saveSync(d: Device) {
		const mine = opened;
		const sent = JSON.stringify(syncDraft);
		const width = syncDraft.maxWidth;
		const settings: DeviceSettings = {
			// Its own left blank is refused, not taken for the Library's.
			structure: syncDraft.own ? syncDraft.structure : null,
			fat_safe: syncDraft.fatSafe,
			art: {
				cover_file: syncDraft.coverFile,
				cover_name: syncDraft.coverName,
				max_width: width === null || Number.isNaN(width) ? null : Math.round(width)
			}
		};
		const saved = await devices.saveSync(d.id, settings);
		// As with renaming: only these settings as they were saved close.
		const same = mine === opened && syncing === d.id && JSON.stringify(syncDraft) === sent;
		if (saved && same) syncing = null;
	}

	/** What a device's sync settings come to, in a line. */
	function syncSummary(s: DeviceSettings) {
		return [
			s.structure ? `Folders as ${s.structure}` : "The Library's folders",
			s.fat_safe && 'FAT-safe names',
			s.art.cover_file ? `cover file “${s.art.cover_name}”` : 'no cover file',
			s.art.max_width && `art at most ${s.art.max_width} px wide`
		]
			.filter(Boolean)
			.join(' · ');
	}

	async function confirm(d: Device) {
		const asked = confirming;
		if (!asked || asked.id !== d.id) return;
		const done =
			asked.action === 'remove' ? await devices.remove(d.id) : await devices.revoke(d.id);
		// A confirmation opened meanwhile, for this device or another, stays.
		if (done && confirming === asked) confirming = null;
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
										{#if d.sync}
											<p class="meta sync-summary">{syncSummary(d.sync.settings)}</p>
										{/if}
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
													{#if d.sync}
														<button
															class="btn sm"
															onclick={() => (syncing === d.id ? (syncing = null) : editSync(d))}
															aria-expanded={syncing === d.id}
														>
															<SlidersHorizontal size={14} /> Sync settings
														</button>
													{/if}
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
								{#if syncing === d.id && d.sync && devices.canManage}
									<form
										class="sync-edit"
										transition:slide={{ duration: 220, easing: cubicOut }}
										onsubmit={(e) => {
											e.preventDefault();
											saveSync(d);
										}}
									>
										<fieldset>
											<legend class="field-label"><FolderTree size={15} /> Folders</legend>
											<div class="segmented" role="radiogroup" aria-label="Folder structure">
												<label class:on={!syncDraft.own}>
													<input type="radio" value={false} bind:group={syncDraft.own} />
													Like the Library
												</label>
												<label class:on={syncDraft.own}>
													<input type="radio" value={true} bind:group={syncDraft.own} />
													Its own
												</label>
											</div>
											{#if syncDraft.own}
												<StructureField
													bind:value={syncDraft.structure}
													folder=""
													suggestions={setup.suggestions}
													variables={setup.variables}
													label={(id) => setup.label(id)}
													device
												/>
											{:else}
												<p class="help">
													Each file goes where it is in the Library{setup.saved.structure
														? `, ${setup.saved.structure}`
														: ''}.
												</p>
											{/if}
										</fieldset>
										<Toggle
											bind:checked={syncDraft.fatSafe}
											label="FAT-safe names"
											description="For memory cards and players formatted FAT32 or exFAT: names leave out : ? * &quot; < > | and trailing dots and spaces, steer clear of names Windows keeps for itself, such as CON, and stay short."
										/>
										<Toggle
											bind:checked={syncDraft.coverFile}
											label="Cover file"
											description="Each album's cover as an image beside its tracks, for players that look for one."
										/>
										{#if syncDraft.coverFile}
											<label class="inline-field" transition:slide={{ duration: 180 }}>
												<span class="field-label">File name</span>
												<input
													bind:value={syncDraft.coverName}
													placeholder="cover"
													maxlength="64"
													spellcheck="false"
													autocomplete="off"
												/>
												<span class="muted">.jpg or .png, as the image is</span>
											</label>
										{/if}
										<div class="width">
											<label class="inline-field">
												<span class="field-label"><ImageIcon size={15} /> Widest art</span>
												<input
													type="number"
													min="16"
													max="16384"
													step="1"
													inputmode="numeric"
													placeholder="No limit"
													bind:value={syncDraft.maxWidth}
												/>
												<span class="muted">pixels</span>
											</label>
											<p class="help">
												For the cover file and the art inside the tracks. Wider art is scaled down
												when it's copied, keeping its shape; narrower art stays as it is.
											</p>
										</div>
										<div class="actions">
											<button type="button" class="btn sm" onclick={() => (syncing = null)}>
												Cancel
											</button>
											<button class="btn sm primary" disabled={devices.busy[d.id]}>
												{#if devices.busy[d.id]}<span class="spin"><LoaderCircle size={14} /></span
													>{/if}
												Save
											</button>
										</div>
									</form>
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

	.sync-summary {
		color: var(--text-2);
	}
	.sync-edit {
		grid-column: 2 / -1;
		display: grid;
		gap: 12px;
		padding: 4px 0 2px;
	}
	.sync-edit fieldset {
		display: grid;
		gap: 10px;
		min-width: 0;
		margin: 0;
		padding: 0;
		border: 0;
	}
	.sync-edit legend {
		margin-bottom: 10px;
		padding: 0;
	}
	.sync-edit .segmented {
		justify-self: start;
	}
	.inline-field {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px 10px;
	}
	.inline-field input {
		width: 160px;
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
	.inline-field input:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.inline-field .muted {
		font-size: 13px;
	}
	.width {
		display: grid;
		gap: 6px;
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
		.sync-edit {
			grid-column: 1 / -1;
		}
	}
</style>
