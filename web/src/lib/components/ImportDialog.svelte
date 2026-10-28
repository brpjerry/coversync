<script lang="ts">
	import { untrack } from 'svelte';
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import X from '@lucide/svelte/icons/x';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import Copy from '@lucide/svelte/icons/copy';
	import Move from '@lucide/svelte/icons/move';
	import ListOrdered from '@lucide/svelte/icons/list-ordered';
	import ArrowDownToLine from '@lucide/svelte/icons/arrow-down-to-line';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import WandSparkles from '@lucide/svelte/icons/wand-sparkles';
	import ImageIcon from '@lucide/svelte/icons/image';
	import ImagePlus from '@lucide/svelte/icons/image-plus';
	import Trash2 from '@lucide/svelte/icons/trash-2';
	import {
		api,
		ApiError,
		coverUrl,
		type Album,
		type ImportEdits,
		type ImportPlan
	} from '#lib/api.ts';
	import { accent, shade } from '#lib/color.ts';
	import SuggestPanel from './SuggestPanel.svelte';

	interface Props {
		album: Album;
		plan: ImportPlan;
		/** Edits made before the dialog opened (e.g. in the album's tag editor). */
		baseEdits?: ImportEdits;
		label: (id: string) => string;
		/** Kinds asked for once per album rather than per track. */
		albumKinds: string[];
		notEditable: string[];
		/** Core tag kinds, shown up front among suggestions. */
		coreKinds: string[];
		/** Every tag kind, in catalog order. */
		kindOrder: string[];
		onclose: () => void;
		/**
		 * Runs the import (with album art chosen here, base64, and how many
		 * suggested values were accepted); resolves to an error or null.
		 */
		onimport: (edits: ImportEdits, art?: string, suggested?: number) => Promise<string | null>;
	}
	let {
		album,
		plan,
		baseEdits = {},
		label,
		albumKinds,
		notEditable,
		coreKinds,
		kindOrder,
		onclose,
		onimport
	}: Props = $props();

	// The columns are the kinds that failed when the dialog opened, so a
	// field doesn't vanish the moment it's filled in.
	const initial = untrack(() => $state.snapshot(plan)) as ImportPlan;
	const tracks = initial.tracks;
	const failing = initial.failing;
	const albumCols = failing.filter((k) => albumKinds.includes(k));
	const trackCols = failing.filter((k) => !albumKinds.includes(k));
	const original = (id: number, k: string) => tracks.find((t) => t.id === id)?.values[k] ?? '';

	/** The value most tracks already agree on, used to pre-fill album fields. */
	function consensus(k: string) {
		const counts = new Map<string, number>();
		for (const t of tracks) {
			const v = t.values[k]?.trim();
			if (v) counts.set(v, (counts.get(v) ?? 0) + 1);
		}
		return [...counts].sort((a, b) => b[1] - a[1])[0]?.[0] ?? '';
	}
	const prefilled = new Set(albumCols.filter((k) => consensus(k)));
	let albumValues = $state<Record<string, string>>(
		Object.fromEntries(albumCols.map((k) => [k, consensus(k)]))
	);
	let trackValues = $state<Record<number, Record<string, string>>>(
		Object.fromEntries(
			tracks.map((t) => [t.id, Object.fromEntries(trackCols.map((k) => [k, t.values[k] ?? '']))])
		)
	);

	/** Suggestions accepted for tags without a field here, and accepted values in all. */
	let accepted = $state<ImportEdits>({});
	let suggested = $state(0);

	function effective(id: number, k: string) {
		return albumCols.includes(k)
			? albumValues[k].trim() || accepted[id]?.[k] || original(id, k)
			: (trackValues[id]?.[k] ?? '').trim();
	}
	/** Suggestions fill in the fields above as if typed. */
	const getField = (id: number | null, k: string) =>
		id === null ? (albumValues[k] ?? '') : (trackValues[id]?.[k] ?? '');
	function setField(id: number | null, k: string, v: string) {
		if (id === null) albumValues[k] = v;
		else if (trackValues[id]) trackValues[id][k] = v;
	}
	const missingOnTracks = (k: string) => tracks.filter((t) => !t.values[k]?.trim()).length;

	// Album fields count once (one input), track fields once per track.
	let missing = $derived(
		albumCols.filter((k) => tracks.some((t) => !effective(t.id, k))).length +
			tracks.reduce((n, t) => n + trackCols.filter((k) => !effective(t.id, k)).length, 0)
	);
	/** The rules require album art and the album has none. */
	const artNeeded = initial.art.missing;
	const total = albumCols.length + tracks.length * trackCols.length + (artNeeded ? 1 : 0);

	type Upload = {
		url: string;
		data: string;
		name: string;
		type: string;
		width: number;
		height: number;
	};
	let upload = $state<Upload | null>(null);
	let artError = $state<string | null>(null);
	let dragging = $state(false);
	let picker = $state<HTMLInputElement>();
	const ART_TYPES = ['image/jpeg', 'image/png', 'image/webp'];
	const MAX_ART = 32 * 1024 * 1024;

	async function choose(file: File | undefined | null) {
		if (!file) return;
		artError = null;
		if (!ART_TYPES.includes(file.type)) {
			artError = 'Choose a JPEG, PNG or WebP image.';
			return;
		}
		if (file.size > MAX_ART) {
			artError = 'Album art can be at most 32 MB.';
			return;
		}
		const url = await new Promise<string>((resolve, reject) => {
			const r = new FileReader();
			r.onload = () => resolve(r.result as string);
			r.onerror = () => reject(r.error);
			r.readAsDataURL(file);
		}).catch(() => '');
		const img = new Image();
		img.src = url;
		await img.decode().catch(() => {});
		if (!url || !img.naturalWidth) {
			artError = "That image couldn't be read.";
			return;
		}
		upload = {
			url,
			data: url.slice(url.indexOf(',') + 1),
			name: file.name,
			type: file.type,
			width: img.naturalWidth,
			height: img.naturalHeight
		};
	}

	function ondrop(e: DragEvent) {
		e.preventDefault();
		dragging = false;
		choose(e.dataTransfer?.files[0]);
	}

	/** A pasted image works too, wherever the focus is. */
	function onpaste(e: ClipboardEvent) {
		if (!artNeeded) return;
		const file = [...(e.clipboardData?.files ?? [])].find((f) => f.type.startsWith('image/'));
		if (file) {
			e.preventDefault();
			choose(file);
		}
	}

	/** What the import does with the album art, in a sentence. */
	let artNote = $derived.by(() => {
		const a = live.art;
		const from = upload ? 'The image you chose' : a.source;
		if (!from) return null;
		const ext = upload
			? ({ 'image/png': 'png', 'image/webp': 'webp' }[upload.type] ?? 'jpg')
			: null;
		const file = a.placement === 'embedded' ? null : ext ? `${a.file_name}.${ext}` : a.file;
		const embed = a.placement === 'embedded' || a.placement === 'both';
		const into = embed
			? upload
				? 'embedded in every track'
				: a.embed
					? `embedded in ${a.embed} ${a.embed === 1 ? 'track' : 'tracks'} without art`
					: null
			: null;
		const did = [file && `saved as ${file}`, into].filter(Boolean).join(', and ');
		const what = upload ? from : `${from[0].toUpperCase()}${from.slice(1)}`;
		return did ? `${what} is ${did}.` : `${what} is kept as it is.`;
	});

	// The plan's values already include the base edits; what's typed here goes on top.
	const base = untrack(() => $state.snapshot(baseEdits)) as ImportEdits;
	let edits = $derived.by(() => {
		const out: ImportEdits = structuredClone(base);
		for (const [id, values] of Object.entries(accepted)) Object.assign((out[+id] ??= {}), values);
		for (const t of tracks) {
			for (const k of failing) {
				const v = effective(t.id, k);
				if (v && v !== original(t.id, k)) (out[t.id] ??= {})[k] = v;
			}
		}
		return out;
	});

	// Re-plan as values change, for the destination preview and server-side checks.
	let live = $state<ImportPlan>(initial);
	let planning = $state(false);
	let seq = 0;
	$effect(() => {
		const e = $state.snapshot(edits) as ImportEdits;
		const mine = ++seq;
		planning = true;
		const timer = setTimeout(async () => {
			try {
				const p = await api.importPlan(album.id, e);
				if (mine === seq) live = p;
			} catch {
				// Keep the last plan; the import itself re-checks everything.
			} finally {
				if (mine === seq) planning = false;
			}
		}, 350);
		return () => clearTimeout(timer);
	});

	let destDir = $derived.by(() => {
		const dest = live.tracks[0]?.dest;
		return dest ? dest.slice(0, dest.lastIndexOf('/')) : null;
	});

	let busy = $state(false);
	let error = $state<string | null>(null);
	let artMissing = $derived(artNeeded && !upload);
	/** Missing values plus the album art, if it's needed. */
	let outstanding = $derived(missing + (artMissing ? 1 : 0));
	let ready = $derived(outstanding === 0 && live.problems.length === 0 && !busy);

	function statusText() {
		const values = `${missing} ${missing === 1 ? 'value' : 'values'}`;
		if (missing && artMissing) return `${values} and the album art are missing.`;
		if (artMissing) return 'The album art is missing.';
		return `${values} ${missing === 1 ? 'is' : 'are'} missing.`;
	}

	async function submit() {
		if (!ready) return;
		busy = true;
		error = null;
		error = await onimport($state.snapshot(edits) as ImportEdits, upload?.data, suggested);
		busy = false;
	}

	function fillBlanks(k: string) {
		const first = tracks.map((t) => trackValues[t.id][k].trim()).find(Boolean);
		if (!first) return;
		for (const t of tracks) if (!trackValues[t.id][k].trim()) trackValues[t.id][k] = first;
	}
	function numberTracks() {
		tracks.forEach((t, i) => (trackValues[t.id].track_number = String(i + 1)));
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			onclose();
		} else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
			submit();
		}
	}

	let dialog = $state<HTMLElement>();
	$effect(() => {
		dialog
			?.querySelector<HTMLElement>('input.missing, .art-choose')
			?.focus({ preventScroll: true });
	});
</script>

<div
	class="scrim"
	role="presentation"
	onclick={(e) => e.target === e.currentTarget && onclose()}
	{onkeydown}
	{onpaste}
	transition:fade={{ duration: 200 }}
>
	<div
		class="dialog"
		role="dialog"
		aria-modal="true"
		aria-labelledby="import-title"
		bind:this={dialog}
		style:--accent={accent(album.color)}
		style:--tint={shade(album.color, 16)}
		transition:fly={{ y: 24, duration: 360, easing: cubicOut }}
	>
		<header class="head">
			<span class="art">
				{#if upload}<img
						src={upload.url}
						alt=""
						in:fade={{ duration: 240 }}
					/>{:else if album.has_cover}<img src={coverUrl(album.id, 256)} alt="" />{:else}<Disc3
						size={28}
						strokeWidth={1.3}
					/>{/if}
			</span>
			<div class="head-text">
				<span class="eyebrow"><FolderInput size={14} /> Import into Library</span>
				<h2 id="import-title">{album.title}</h2>
				<p>{album.artist}</p>
			</div>
			<button class="icon-btn close" onclick={onclose} aria-label="Close"><X size={19} /></button>
		</header>

		<div class="status" class:done={outstanding === 0}>
			<span class="status-icon">
				{#if outstanding === 0}<CircleCheck size={18} />{:else}<CircleAlert size={18} />{/if}
			</span>
			<span class="status-text">
				{#if outstanding === 0}
					{artNeeded
						? 'Every required tag has a value, and the album has art.'
						: 'Every required tag has a value.'}
				{:else}
					{#key outstanding}<b in:fly={{ y: -6, duration: 200 }}>{statusText()}</b>{/key}
					The library rules{trackCols.length || albumCols.length ? ' and folder structure' : ''} need
					{outstanding === 1 ? 'it' : 'them'} before this album can be imported.
				{/if}
			</span>
			<span class="meter"
				><span style:transform="scaleX({total ? 1 - outstanding / total : 1})"></span></span
			>
		</div>

		<div class="body">
			{#if initial.suggest}
				<SuggestPanel
					albumId={album.id}
					{tracks}
					{base}
					{label}
					order={kindOrder}
					{albumKinds}
					{coreKinds}
					fields={{ track: trackCols, album: albumCols }}
					{getField}
					{setField}
					onaccept={(e, n) => {
						accepted = e;
						suggested = n;
					}}
				/>
			{/if}
			{#if live.autofilled.length}
				<p class="autofilled">
					<WandSparkles size={14} />
					<span>
						Filled in automatically:
						{live.autofilled.map((k) => label(k)).join(', ')}. Change it in Inbox setup.
					</span>
				</p>
			{/if}
			{#if artNote}
				<p class="autofilled">
					<ImageIcon size={14} />
					<span>{artNote} Change it in Library setup.</span>
				</p>
			{/if}
			{#if live.problems.length}
				<ul class="problems" transition:fly={{ y: -6, duration: 200 }}>
					{#each live.problems as p (p)}<li><TriangleAlert size={15} /> {p}</li>{/each}
				</ul>
			{/if}

			{#if artNeeded}
				<section>
					<h3>Album art</h3>
					<div
						class="art-drop"
						class:missing={artMissing}
						class:over={dragging}
						role="group"
						aria-label="Album art"
						ondragover={(e) => {
							e.preventDefault();
							dragging = true;
						}}
						ondragleave={() => (dragging = false)}
						{ondrop}
					>
						{#if upload}
							<img class="art-preview" src={upload.url} alt="Chosen album art" in:fade />
							<span class="art-text">
								<b>{upload.name}</b>
								<span class="tabular">{upload.width} × {upload.height}</span>
							</span>
							<span class="art-actions">
								<button class="btn art-choose" onclick={() => picker?.click()}>
									<ImagePlus size={14} /> Choose another
								</button>
								<button
									class="icon-btn"
									onclick={() => (upload = null)}
									aria-label="Remove the chosen image"><Trash2 size={16} /></button
								>
							</span>
						{:else}
							<span class="art-empty"><ImagePlus size={24} strokeWidth={1.6} /></span>
							<span class="art-text">
								<b>This album has no cover art.</b>
								<span
									>The library rules require it. Drop or paste an image here, or choose one.</span
								>
								{#if artError}<span class="art-error" transition:fade>{artError}</span>{/if}
							</span>
							<span class="art-actions">
								<button class="btn art-choose" onclick={() => picker?.click()}>
									<ImagePlus size={14} /> Choose image
								</button>
							</span>
						{/if}
						<input
							bind:this={picker}
							type="file"
							accept={ART_TYPES.join(',')}
							hidden
							onchange={(e) => {
								choose(e.currentTarget.files?.[0]);
								e.currentTarget.value = '';
							}}
						/>
					</div>
				</section>
			{/if}

			{#if albumCols.length}
				<section>
					<h3>Album</h3>
					<div class="album-fields">
						{#each albumCols as k (k)}
							{@const empty = !albumValues[k].trim() && tracks.some((t) => !original(t.id, k))}
							<label class="field">
								<span class="field-label">
									{label(k)}
									<span class="note" class:bad={empty}>
										{#if empty}
											missing on {missingOnTracks(k)} of {tracks.length}
										{:else if prefilled.has(k) && albumValues[k] === consensus(k)}
											filled in from the other tracks
										{/if}
									</span>
								</span>
								<input
									class:missing={empty}
									bind:value={albumValues[k]}
									disabled={notEditable.includes(k)}
									placeholder={notEditable.includes(k)
										? "Can't be typed in"
										: `Applies to all ${tracks.length} tracks`}
									spellcheck="false"
								/>
							</label>
						{/each}
					</div>
				</section>
			{/if}

			{#if trackCols.length}
				<section>
					<h3>Tracks</h3>
					<div class="table" style:--cols={trackCols.length}>
						<div class="thead">
							<span class="file-col">File</span>
							{#each trackCols as k (k)}
								<span class="col-head">
									{label(k)}
									{#if k === 'track_number'}
										<button
											class="mini"
											onclick={numberTracks}
											title="Number the tracks 1 to {tracks.length}"
										>
											<ListOrdered size={13} /> 1–{tracks.length}
										</button>
									{:else}
										<button
											class="mini"
											onclick={() => fillBlanks(k)}
											title="Copy the first value into empty cells"
										>
											<ArrowDownToLine size={13} /> Fill blanks
										</button>
									{/if}
								</span>
							{/each}
						</div>
						{#each tracks as t, i (t.id)}
							<div class="tr" style:--i={Math.min(i, 20)}>
								<span class="file-col">
									<span class="file-title">{t.title}</span>
									<span class="file-name">{t.file_name}</span>
								</span>
								{#each trackCols as k (k)}
									<input
										class:missing={!trackValues[t.id][k].trim()}
										bind:value={trackValues[t.id][k]}
										disabled={notEditable.includes(k)}
										placeholder={notEditable.includes(k) ? "Can't be typed in" : 'Required'}
										aria-label="{label(k)}, {t.title}"
										inputmode={k.endsWith('_number') || k.endsWith('_total')
											? 'numeric'
											: undefined}
										spellcheck="false"
									/>
								{/each}
							</div>
						{/each}
					</div>
				</section>
			{/if}
		</div>

		<footer class="foot">
			<div class="dest">
				<span class="mode">
					{#if live.mode === 'copy'}<Copy size={13} /> Copy · the original stays untouched{:else}<Move
							size={13}
						/> Move · the original leaves the Inbox{/if}
				</span>
				<span class="dest-path" title={destDir ?? ''}>
					{#if planning}<span class="spin"><LoaderCircle size={13} /></span>{/if}
					{destDir ?? 'Destination appears once the folder-structure tags are filled in'}
				</span>
				{#if error}<span class="error" transition:fade>{error}</span>{/if}
			</div>
			<div class="buttons">
				<button class="btn" onclick={onclose}>Cancel</button>
				<button class="btn primary" onclick={submit} disabled={!ready}>
					{#if busy}<span class="spin"><LoaderCircle size={15} /></span>{:else}<FolderInput
							size={15}
						/>{/if}
					Import
				</button>
			</div>
		</footer>
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
		width: min(960px, 100%);
		max-height: calc(100dvh - 48px);
		display: grid;
		grid-template-rows: auto auto minmax(0, 1fr) auto;
		border-radius: 24px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow: 0 40px 120px -30px rgb(0 0 0 / 0.75);
		overflow: hidden;
	}
	.head {
		display: flex;
		align-items: center;
		gap: 16px;
		padding: 20px 22px 16px;
		background: linear-gradient(to bottom, var(--tint), transparent);
	}
	.art {
		flex: none;
		display: grid;
		place-items: center;
		width: 72px;
		height: 72px;
		border-radius: 12px;
		overflow: hidden;
		background: var(--surface-hover);
		color: var(--text-3);
		box-shadow: 0 10px 24px -10px rgb(0 0 0 / 0.7);
	}
	.art img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.head-text {
		flex: 1;
		min-width: 0;
	}
	.eyebrow {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--accent);
	}
	h2 {
		margin: 4px 0 0;
		font-size: 22px;
		letter-spacing: -0.02em;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.head-text p {
		margin: 0;
		color: var(--text-2);
	}
	.close {
		align-self: flex-start;
	}

	.status {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 6px 10px;
		align-items: center;
		margin: 0 22px;
		padding: 12px 14px;
		border-radius: 14px;
		background: color-mix(in oklab, var(--danger) 8%, var(--surface));
		border: 1px solid color-mix(in oklab, var(--danger) 30%, transparent);
		font-size: 13.5px;
		color: var(--text-2);
		transition:
			background 300ms,
			border-color 300ms;
	}
	.status.done {
		background: color-mix(in oklab, var(--success) 8%, var(--surface));
		border-color: color-mix(in oklab, var(--success) 35%, transparent);
	}
	.status-icon {
		display: grid;
		color: var(--danger);
	}
	.done .status-icon {
		color: var(--success);
	}
	.status-text b {
		display: inline-block;
		color: var(--text);
	}
	.meter {
		grid-column: 1 / -1;
		height: 4px;
		border-radius: 999px;
		background: var(--border);
		overflow: hidden;
	}
	.meter span {
		display: block;
		height: 100%;
		background: var(--success);
		transform-origin: left;
		transition: transform 400ms var(--ease-out);
	}

	.body {
		overflow-y: auto;
		/* One column that may shrink below its content's widest line: long
		   release names wrap or truncate instead of widening the dialog. */
		grid-template-columns: minmax(0, 1fr);
		scrollbar-width: thin;
		scrollbar-color: var(--border-strong) transparent;
		padding: 16px 22px 8px;
		display: grid;
		gap: 20px;
		align-content: start;
	}
	h3 {
		margin: 0 0 10px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	.autofilled {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		margin: 0;
		font-size: 13px;
		color: var(--text-2);
	}
	.autofilled :global(svg) {
		flex: none;
		margin-top: 2px;
		color: var(--accent);
	}
	.art-drop {
		display: flex;
		align-items: center;
		gap: 14px;
		padding: 12px 14px;
		border-radius: 14px;
		border: 1px dashed var(--border-strong);
		background: var(--surface);
		transition:
			border-color 160ms,
			background 160ms;
	}
	.art-drop.missing {
		border-color: color-mix(in oklab, var(--danger) 55%, transparent);
	}
	.art-drop.over {
		border-style: solid;
		border-color: var(--accent);
		background: color-mix(in oklab, var(--accent) 10%, var(--surface));
	}
	.art-empty {
		display: grid;
		place-items: center;
		flex: none;
		width: 56px;
		height: 56px;
		border-radius: 10px;
		background: var(--surface-hover);
		color: var(--text-3);
	}
	.art-preview {
		flex: none;
		width: 56px;
		height: 56px;
		border-radius: 10px;
		object-fit: cover;
		box-shadow: 0 8px 18px -10px rgb(0 0 0 / 0.7);
	}
	.art-text {
		flex: 1;
		min-width: 0;
		display: grid;
		gap: 2px;
		font-size: 13px;
		color: var(--text-2);
	}
	.art-text b {
		color: var(--text);
		font-weight: 580;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.art-error {
		color: var(--danger);
	}
	.art-actions {
		display: flex;
		align-items: center;
		gap: 6px;
		flex: none;
	}
	.art-choose {
		height: 32px;
		padding: 0 12px;
		font-size: 13px;
	}
	.problems {
		list-style: none;
		margin: 0;
		padding: 10px 14px;
		display: grid;
		gap: 6px;
		border-radius: 12px;
		background: color-mix(in oklab, hsl(40 90% 60%) 10%, transparent);
		border: 1px solid color-mix(in oklab, hsl(40 90% 60%) 30%, transparent);
		color: color-mix(in oklab, hsl(40 90% 70%) 85%, var(--text));
		font-size: 13.5px;
	}
	.problems li {
		display: flex;
		gap: 8px;
		align-items: flex-start;
	}
	.problems :global(svg) {
		flex: none;
		margin-top: 2px;
	}

	.album-fields {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
		gap: 12px 14px;
	}
	.field {
		display: grid;
		gap: 6px;
	}
	.field-label {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
		font-size: 12.5px;
		font-weight: 550;
		color: var(--text-2);
	}
	.note {
		font-weight: 500;
		font-size: 11.5px;
		color: var(--accent);
	}
	.note.bad {
		color: var(--danger);
	}

	input {
		width: 100%;
		height: 38px;
		padding: 0 12px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface);
		outline: none;
		font-size: 14px;
		transition:
			border-color 200ms,
			background 200ms,
			box-shadow 200ms;
	}
	input:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	/* Failures stand out, with one pulse when the dialog opens. */
	input.missing {
		border-color: color-mix(in oklab, var(--danger) 70%, transparent);
		background: color-mix(in oklab, var(--danger) 7%, var(--surface));
		animation: flag 1.2s var(--ease-out) 250ms 1;
	}
	input.missing::placeholder {
		color: color-mix(in oklab, var(--danger) 70%, var(--text-3));
	}
	input:disabled {
		opacity: 0.6;
		cursor: not-allowed;
	}
	@keyframes flag {
		30% {
			box-shadow: 0 0 0 5px color-mix(in oklab, var(--danger) 25%, transparent);
		}
	}

	.table {
		display: grid;
		gap: 6px;
	}
	.thead,
	.tr {
		display: grid;
		grid-template-columns: minmax(160px, 1.2fr) repeat(var(--cols), minmax(150px, 1fr));
		gap: 8px;
		align-items: center;
	}
	.thead {
		font-size: 12px;
		color: var(--text-3);
	}
	.col-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 6px;
		padding: 0 4px;
	}
	.mini {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 2px 8px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-2);
		background: var(--surface-hover);
	}
	.mini:hover {
		color: var(--text);
	}
	.tr {
		animation: row-in 360ms var(--ease-out) both;
		animation-delay: calc(var(--i) * 18ms);
	}
	@keyframes row-in {
		from {
			opacity: 0;
			transform: translateY(6px);
		}
	}
	.file-col {
		display: grid;
		min-width: 0;
	}
	.file-title {
		font-size: 13.5px;
		font-weight: 520;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.file-name {
		font-size: 11.5px;
		color: var(--text-3);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.foot {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		padding: 14px 22px 18px;
		border-top: 1px solid var(--border);
	}
	.dest {
		display: grid;
		gap: 3px;
		min-width: 0;
		font-size: 12.5px;
	}
	.mode {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		color: var(--text-2);
	}
	.dest-path {
		display: flex;
		align-items: center;
		gap: 6px;
		color: var(--text-3);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.error {
		color: var(--danger);
	}
	.buttons {
		display: flex;
		gap: 8px;
		flex: none;
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
		.scrim {
			padding: 0;
		}
		.dialog {
			max-height: 100dvh;
			height: 100dvh;
			border-radius: 0;
		}
		.foot {
			flex-direction: column;
			align-items: stretch;
		}
		.table {
			overflow-x: auto;
		}
	}
</style>
