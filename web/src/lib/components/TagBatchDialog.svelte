<script lang="ts">
	import { onDestroy } from 'svelte';
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import { SvelteMap } from 'svelte/reactivity';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import FolderTree from '@lucide/svelte/icons/folder-tree';
	import { api, coverUrl, type Album, type AlbumTags } from '#lib/api.ts';
	import { count } from '#lib/format.ts';
	import {
		BATCH_KINDS,
		slim,
		tally,
		type BatchKind,
		type BatchSet,
		type SaveJob
	} from '#lib/tag-batch.svelte.ts';

	interface Props {
		/** The albums to edit, in the grid's order. */
		albums: Album[];
		/** Library setup's collections. */
		collections: string[];
		/** Values to suggest for the text fields: those already in the Library. */
		suggestions: Record<'album_artist' | 'genre', string[]>;
		/** Library setup needs the tag (a required tag, or in the folder structure): it can't be emptied. */
		needed: (kind: BatchKind) => boolean;
		/** The tag is in the folder structure: changing it moves files. */
		moves: (kind: BatchKind) => boolean;
		onsave: (jobs: SaveJob[]) => void;
		onclose: () => void;
	}
	let { albums, collections, suggestions, needed, moves, onsave, onclose }: Props = $props();

	const LABEL: Record<BatchKind, string> = {
		collection: 'Collection',
		album_artist: 'Album artist',
		genre: 'Genre'
	};
	const COVERS = 8;
	/** For the fields' ids: two dialogs are up at once while one closes and the next opens. */
	const uid = $props.id();

	/** Each album's tags as read when the dialog opened: the saves are made on these. */
	const read = new SvelteMap<string, AlbumTags>();
	let unread = $state(0);
	let reading = $state(true);

	/** What each field holds: the value every album shares, or nothing when they differ. */
	let start = $state<Record<BatchKind, string | null>>({
		collection: null,
		album_artist: '',
		genre: ''
	});
	/** Collection null: Keep each album's. */
	let values = $state<Record<BatchKind, string | null>>({
		collection: null,
		album_artist: '',
		genre: ''
	});
	/** Not every album has the same value (or one couldn't be read). */
	let mixed = $state<Record<BatchKind, boolean>>({
		collection: true,
		album_artist: true,
		genre: true
	});

	let counts = $derived(
		Object.fromEntries(BATCH_KINDS.map((k) => [k, tally([...read.values()], k)])) as Record<
			BatchKind,
			ReturnType<typeof tally>
		>
	);

	/** Closed: no more albums are read. */
	let closed = false;
	onDestroy(() => (closed = true));

	async function readAll() {
		// A few at a time: each is a local read of the album's files.
		let next = 0;
		const worker = async () => {
			while (!closed && next < albums.length) {
				const album = albums[next++];
				try {
					read.set(album.id, slim(await api.albumTags(album.id)));
				} catch {
					unread++;
				}
			}
		};
		await Promise.all(Array.from({ length: Math.min(4, albums.length) }, worker));
		if (closed) return;
		for (const kind of BATCH_KINDS) {
			const found = tally([...read.values()], kind);
			const one = !unread && found.length === 1 && found[0].value !== null;
			mixed[kind] = !one;
			start[kind] = one ? found[0].value : kind === 'collection' ? null : '';
			values[kind] = start[kind];
		}
		reading = false;
	}
	readAll();

	/** A value as saving writes it: trimmed of white space as the daemon trims it (U+0085 too). */
	const clean = (v: string | null) =>
		v?.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '') ?? null;
	/** Whether the field was changed: a mixed text field left empty keeps each album's own. */
	function changed(kind: BatchKind): boolean {
		const value = clean(values[kind]);
		if (value === clean(start[kind])) return false;
		return !(mixed[kind] && kind !== 'collection' && !value);
	}
	/** Emptied (or None), though Library setup needs it. */
	const emptied = (kind: BatchKind) => changed(kind) && !clean(values[kind]) && needed(kind);

	let edited = $derived(BATCH_KINDS.filter(changed));
	let blocked = $derived(BATCH_KINDS.some(emptied));
	let moving = $derived(edited.filter(moves).map((k) => LABEL[k]));

	/** The collections to pick from: Library setup's, then any a track is in. */
	let choices = $derived([
		...new Set([
			...collections,
			...[...read.values()].flatMap((tags) =>
				tags.tracks.map((t) => t.values.collection).filter(Boolean)
			)
		])
	]);

	/** A value as the counts name it. */
	function named(kind: BatchKind, value: string | null) {
		if (value === null) return 'differs within the album';
		if (value === '') return kind === 'collection' ? 'None' : 'empty';
		return value;
	}
	function summary(kind: BatchKind) {
		const all = counts[kind].map((c) => `${named(kind, c.value)} ${count(c.albums)}`);
		return {
			shown: all.slice(0, 3).join(' · '),
			more: all.length - 3,
			rest: all.slice(3).join(' · ')
		};
	}

	function save() {
		if (reading || !edited.length || blocked) return;
		const set: BatchSet = {};
		for (const kind of edited) set[kind] = clean(values[kind]) ?? '';
		closed = true;
		onsave(albums.map((album) => ({ album, set, tags: read.get(album.id) ?? null })));
	}

	function cancel() {
		if (edited.length && !confirm('Discard these tag changes?')) return;
		// No more reads, though it takes a moment to close.
		closed = true;
		onclose();
	}

	function onkeydown(e: KeyboardEvent) {
		// Keys go to the dialog, not the page under it.
		if (e.key === 'Escape') {
			e.stopPropagation();
			cancel();
		} else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
			e.preventDefault();
			e.stopPropagation();
			save();
		}
	}

	let dialog = $state<HTMLElement>();
	$effect(() => dialog?.focus({ preventScroll: true }));
</script>

<div
	class="scrim"
	role="presentation"
	onclick={(e) => e.target === e.currentTarget && cancel()}
	{onkeydown}
	transition:fade={{ duration: 200 }}
>
	<div
		class="dialog"
		role="dialog"
		aria-modal="true"
		aria-labelledby="{uid}-title"
		tabindex="-1"
		bind:this={dialog}
		transition:fly={{ y: 24, duration: 360, easing: cubicOut }}
	>
		<header>
			<p class="eyebrow">Edit tags</p>
			<h2 id="{uid}-title">
				{count(albums.length)}
				{albums.length === 1 ? 'album' : 'albums'}
			</h2>
			<div class="covers" aria-hidden="true">
				{#each albums.slice(0, COVERS) as album (album.id)}
					<span class="art" title={album.title}>
						{#if album.has_cover}<img
								src={coverUrl(album, 256)}
								alt=""
								onerror={(e) => ((e.currentTarget as HTMLImageElement).hidden = true)}
							/>{:else}<Disc3 size={16} strokeWidth={1.4} />{/if}
					</span>
				{/each}
				{#if albums.length > COVERS}<span class="more tabular"
						>+{count(albums.length - COVERS)}</span
					>{/if}
			</div>
		</header>

		<div class="body">
			{#if reading}
				<p class="status">
					<span class="spin"><LoaderCircle size={15} /></span>
					Reading {count(albums.length)}
					{albums.length === 1 ? 'album' : 'albums'}…
				</p>
			{:else}
				{#if unread}
					<p class="status warn">
						{count(unread)}
						{unread === 1 ? "album's tags couldn't" : "albums' tags couldn't"} be read: they're read again
						when saved, and aren't counted below.
					</p>
				{/if}
				<div class="fields">
					{#each BATCH_KINDS as kind (kind)}
						{@const sum = summary(kind)}
						<div class="field" class:changed={changed(kind)} class:invalid={emptied(kind)}>
							<label class="label" for="{uid}-{kind}"
								>{LABEL[kind]}{#if changed(kind)}<i class="dot" in:fly={{ y: -4 }}></i>{/if}</label
							>
							{#if kind === 'collection'}
								<select id="{uid}-{kind}" bind:value={values.collection}>
									{#if mixed.collection}<option value={null}>Keep each album's</option>{/if}
									<option value="">None</option>
									{#each choices as c (c)}<option value={c}>{c}</option>{/each}
								</select>
							{:else}
								<input
									id="{uid}-{kind}"
									bind:value={values[kind]}
									list="{uid}-{kind}-values"
									placeholder={mixed[kind] ? 'Mixed — leave empty to keep' : ''}
									spellcheck="false"
									autocomplete="off"
								/>
								<datalist id="{uid}-{kind}-values">
									{#each suggestions[kind] as v (v)}<option value={v}></option>{/each}
								</datalist>
							{/if}
							{#if emptied(kind)}
								<span class="note error"
									>{LABEL[kind]} can't be {kind === 'collection' ? 'None' : 'left empty'}: Library
									setup needs it.</span
								>
							{:else if sum.more > 0}
								<details class="note tabular">
									<summary
										>{changed(kind) ? 'Was ' : ''}{sum.shown} · +{count(sum.more)} more</summary
									>
									{sum.rest}
								</details>
							{:else if sum.shown}
								<span class="note tabular">{changed(kind) ? 'Was ' : ''}{sum.shown}</span>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		</div>

		<footer>
			<p class="moves">
				{#if moving.length}
					<FolderTree size={14} />
					<span
						>{moving.join(' and ')}
						{moving.length === 1 ? 'is' : 'are'} in the folder structure: albums move to fit when they're
						saved.</span
					>
				{/if}
			</p>
			<div class="buttons">
				<button class="btn" onclick={cancel}>Cancel</button>
				<button
					class="btn primary"
					onclick={save}
					disabled={reading || !edited.length || blocked}
					title="Save (Ctrl+Enter)"
				>
					Save {count(albums.length)}
					{albums.length === 1 ? 'album' : 'albums'}
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
		grid-template-columns: minmax(0, 1fr);
		place-items: center;
		padding: 24px;
		background: rgb(0 0 0 / 0.5);
		backdrop-filter: blur(10px);
	}
	.dialog {
		width: min(560px, 100%);
		max-height: calc(100dvh - 48px);
		display: grid;
		grid-template-rows: auto minmax(0, 1fr) auto;
		border-radius: 24px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow: 0 40px 120px -30px rgb(0 0 0 / 0.75);
		overflow: hidden;
		outline: none;
	}
	header {
		padding: 22px 24px 8px;
	}
	.eyebrow {
		margin: 0;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	h2 {
		margin: 2px 0 0;
		font-size: 22px;
		letter-spacing: -0.02em;
	}
	.covers {
		display: flex;
		align-items: center;
		gap: 6px;
		margin-top: 14px;
		min-width: 0;
		overflow: hidden;
	}
	.art {
		flex: none;
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: 8px;
		overflow: hidden;
		color: var(--text-3);
		background: var(--surface);
	}
	.art img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.more {
		flex: none;
		font-size: 12.5px;
		color: var(--text-2);
		padding-left: 4px;
	}
	.body {
		padding: 12px 24px 18px;
		overflow-y: auto;
	}
	.status {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 0 0 14px;
		color: var(--text-2);
	}
	.status.warn {
		color: var(--warn);
		font-size: 13px;
	}
	.fields {
		display: grid;
		gap: 16px;
	}
	.field {
		display: grid;
		gap: 6px;
	}
	.label {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
		font-weight: 550;
		color: var(--text-2);
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 999px;
		background: var(--accent);
	}
	input,
	select {
		width: 100%;
		height: 40px;
		padding: 0 12px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text);
		font: inherit;
		outline: none;
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	input::placeholder {
		color: var(--text-3);
		font-style: italic;
	}
	input:hover,
	select:hover {
		border-color: var(--border-strong);
	}
	input:focus,
	select:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.changed input,
	.changed select {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
	}
	.invalid input,
	.invalid select {
		border-color: var(--danger);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--danger) 18%, transparent);
	}
	.note {
		font-size: 12.5px;
		color: var(--text-3);
		overflow-wrap: anywhere;
	}
	summary {
		cursor: pointer;
	}
	summary:hover {
		color: var(--text-2);
	}
	.note.error {
		color: var(--danger);
	}
	footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 14px 20px;
		border-top: 1px solid var(--border);
	}
	.moves {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		margin: 0;
		font-size: 12.5px;
		color: var(--text-2);
	}
	.moves :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	.buttons {
		display: flex;
		gap: 8px;
		margin-left: auto;
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
			width: 100%;
			height: 100dvh;
			max-height: 100dvh;
			border-radius: 0;
		}
		footer {
			flex-wrap: wrap;
		}
	}
</style>
