<script lang="ts">
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { fly } from 'svelte/transition';
	import {
		api,
		ApiError,
		type AlbumDetail,
		type ImportEdits,
		type TagEdit,
		type TagField,
		type TagWriteResult,
		type Track
	} from '#lib/api.ts';
	import { toast } from '#lib/toast.svelte.ts';

	interface Props {
		detail: AlbumDetail;
		/** Whether these files may be saved in place; Save is hidden otherwise. */
		allowWrites: boolean;
		onsaved: (result: TagWriteResult) => void;
		oncancel: () => void;
		/** Inbox albums: import with the current edits applied to the Library copy. */
		onimport?: (edits: ImportEdits) => void;
	}
	let { detail, allowWrites, onsaved, oncancel, onimport }: Props = $props();

	type AlbumField = 'album' | 'album_artist' | 'year' | 'genre';
	type TrackField = 'track_no' | 'disc_no' | 'title' | 'artist';

	const albumFields: { key: AlbumField; label: string; wide?: boolean }[] = [
		{ key: 'album', label: 'Album', wide: true },
		{ key: 'album_artist', label: 'Album artist', wide: true },
		{ key: 'year', label: 'Year' },
		{ key: 'genre', label: 'Genre' }
	];
	const trackFields: { key: TrackField; label: string }[] = [
		{ key: 'track_no', label: '#' },
		{ key: 'disc_no', label: 'Disc' },
		{ key: 'title', label: 'Title' },
		{ key: 'artist', label: 'Artist' }
	];

	function value(t: Track, key: TagField): string {
		const v = t[key as keyof Track];
		return v == null ? '' : String(v);
	}

	/** The shared value of a field across all tracks, or null when they differ. */
	function shared(key: TagField): string | null {
		const values = new Set(detail.tracks.map((t) => value(t, key)));
		return values.size === 1 ? [...values][0] : null;
	}

	function initialAlbum() {
		return Object.fromEntries(albumFields.map((f) => [f.key, shared(f.key) ?? ''])) as Record<
			AlbumField,
			string
		>;
	}
	function initialTracks() {
		return Object.fromEntries(
			detail.tracks.map((t) => [
				t.id,
				Object.fromEntries(trackFields.map((f) => [f.key, value(t, f.key)]))
			])
		) as Record<number, Record<TrackField, string>>;
	}

	const mixed = Object.fromEntries(
		albumFields.map((f) => [f.key, shared(f.key) === null])
	) as Record<AlbumField, boolean>;
	let base = $state({ album: initialAlbum(), tracks: initialTracks() });
	let form = $state(structuredClone($state.snapshot(base)));
	let saving = $state(false);

	const albumChanged = (k: AlbumField) => form.album[k] !== base.album[k];
	const trackChanged = (id: number, k: TrackField) => form.tracks[id][k] !== base.tracks[id][k];

	const invalid = (k: TagField, v: string) => {
		if (!v.trim()) return false;
		if (k === 'track_no' || k === 'disc_no') return !/^\d+(\/\d+)?$/.test(v.trim());
		if (k === 'year') return !/^\d{4}(-\d{2}(-\d{2})?)?$/.test(v.trim());
		return false;
	};

	let changes = $derived(
		albumFields.filter((f) => albumChanged(f.key)).length +
			detail.tracks.reduce(
				(n, t) => n + trackFields.filter((f) => trackChanged(t.id, f.key)).length,
				0
			)
	);
	let errors = $derived(
		albumFields.some((f) => invalid(f.key, form.album[f.key])) ||
			detail.tracks.some((t) => trackFields.some((f) => invalid(f.key, form.tracks[t.id][f.key])))
	);

	function revert() {
		form = structuredClone($state.snapshot(base));
	}

	/** Editor fields as library rule tag kinds, for the import. */
	const KIND: Record<TagField, string> = {
		album: 'album',
		album_artist: 'album_artist',
		year: 'date',
		genre: 'genre',
		track_no: 'track_number',
		disc_no: 'disc_number',
		title: 'title',
		artist: 'artist'
	};

	function importEdits(): ImportEdits {
		const out: ImportEdits = {};
		for (const t of detail.tracks) {
			const edit: Record<string, string> = {};
			for (const f of albumFields) if (albumChanged(f.key)) edit[KIND[f.key]] = form.album[f.key];
			for (const f of trackFields)
				if (trackChanged(t.id, f.key)) edit[KIND[f.key]] = form.tracks[t.id][f.key];
			if (Object.keys(edit).length) out[t.id] = edit;
		}
		return out;
	}

	async function save() {
		const album: TagEdit = {};
		for (const f of albumFields) if (albumChanged(f.key)) album[f.key] = form.album[f.key];
		const tracks: Record<number, TagEdit> = {};
		for (const t of detail.tracks) {
			const edit: TagEdit = {};
			for (const f of trackFields)
				if (trackChanged(t.id, f.key)) edit[f.key] = form.tracks[t.id][f.key];
			if (Object.keys(edit).length) tracks[t.id] = edit;
		}
		saving = true;
		try {
			const result = await api.writeTags(detail.id, { album, tracks });
			toast(
				`Saved tags to ${result.files_written} ${result.files_written === 1 ? 'file' : 'files'}`,
				'success'
			);
			onsaved(result);
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
		} finally {
			saving = false;
		}
	}
</script>

<div class="editor">
	<section>
		<h3>Album</h3>
		<div class="album-grid">
			{#each albumFields as f (f.key)}
				<label
					class="field"
					class:wide={f.wide}
					class:changed={albumChanged(f.key)}
					class:invalid={invalid(f.key, form.album[f.key])}
				>
					<span class="label"
						>{f.label}{#if albumChanged(f.key)}<i class="dot" in:fly={{ y: -4 }}></i>{/if}</span
					>
					<input
						bind:value={form.album[f.key]}
						placeholder={mixed[f.key] ? 'Mixed — leave empty to keep' : ''}
						spellcheck="false"
					/>
				</label>
			{/each}
		</div>
	</section>

	<section>
		<h3>Tracks</h3>
		<div class="table" role="table" aria-label="Track tags">
			<div class="thead" role="row">
				{#each trackFields as f (f.key)}<span role="columnheader" class="col-{f.key}"
						>{f.label}</span
					>{/each}
			</div>
			{#each detail.tracks as t, i (t.id)}
				<div class="tr" role="row" style:--i={Math.min(i, 24)}>
					{#each trackFields as f (f.key)}
						<input
							class="cell col-{f.key}"
							class:changed={trackChanged(t.id, f.key)}
							class:invalid={invalid(f.key, form.tracks[t.id][f.key])}
							bind:value={form.tracks[t.id][f.key]}
							aria-label="{f.label}, track {i + 1}"
							inputmode={f.key === 'track_no' || f.key === 'disc_no' ? 'numeric' : undefined}
							spellcheck="false"
						/>
					{/each}
				</div>
			{/each}
		</div>
	</section>

	<div class="savebar" class:dirty={changes > 0}>
		<span class="summary tabular">
			{#if changes > 0}
				{#key changes}<b in:fly={{ y: 6, duration: 200 }}>{changes}</b>{/key}
				{changes === 1 ? 'change' : 'changes'}{allowWrites ? ' not saved' : ''}
				{#if onimport}<span class="hint"
						>· {allowWrites ? 'or ' : ''}written to the Library copy on import</span
					>{/if}
			{:else}
				No changes
			{/if}
		</span>
		<div class="buttons">
			<button class="btn" onclick={changes ? revert : oncancel}>
				{#if changes}<Undo2 size={15} /> Revert{:else}Done{/if}
			</button>
			{#if allowWrites}
				<button
					class="btn"
					class:primary={!onimport}
					onclick={save}
					disabled={changes === 0 || errors || saving}
				>
					{#if saving}<span class="spin"><LoaderCircle size={15} /></span>{:else}<Check
							size={15}
						/>{/if}
					Save
				</button>
			{/if}
			{#if onimport}
				<button
					class="btn primary"
					onclick={() => onimport(importEdits())}
					disabled={errors || saving}
				>
					<FolderInput size={15} /> Import
				</button>
			{/if}
		</div>
	</div>
</div>

<style>
	.editor {
		display: grid;
		gap: 26px;
		padding: 4px 16px 0;
	}
	h3 {
		margin: 0 0 12px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}

	.album-grid {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 14px;
	}
	.field {
		display: grid;
		gap: 6px;
	}
	.field.wide {
		grid-column: span 2;
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
	input {
		width: 100%;
		height: 40px;
		padding: 0 12px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface);
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
	input:hover {
		border-color: var(--border-strong);
	}
	input:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.changed input,
	input.changed {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
	}
	.invalid input,
	input.invalid {
		border-color: var(--danger);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--danger) 18%, transparent);
	}

	.table {
		display: grid;
		gap: 6px;
	}
	.thead,
	.tr {
		display: grid;
		grid-template-columns: 64px 64px minmax(0, 1.4fr) minmax(0, 1fr);
		gap: 8px;
	}
	.thead span {
		font-size: 12px;
		color: var(--text-3);
		padding: 0 12px;
	}
	.tr {
		animation: row-in 420ms var(--ease-out) both;
		animation-delay: calc(var(--i) * 18ms);
	}
	@keyframes row-in {
		from {
			opacity: 0;
			transform: translateY(6px);
		}
	}
	.cell {
		height: 36px;
		font-size: 14px;
	}
	.col-track_no,
	.col-disc_no {
		text-align: center;
	}

	.savebar {
		position: sticky;
		bottom: 16px;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		padding: 10px 10px 10px 18px;
		border-radius: 999px;
		background: color-mix(in oklab, var(--bg-raised) 85%, transparent);
		backdrop-filter: blur(18px);
		border: 1px solid var(--border);
		box-shadow: var(--shadow);
		transition:
			border-color 240ms,
			box-shadow 240ms;
	}
	.savebar.dirty {
		border-color: color-mix(in oklab, var(--accent) 45%, transparent);
		box-shadow:
			var(--shadow),
			0 0 0 4px color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.summary {
		font-size: 13.5px;
		color: var(--text-2);
	}
	.hint {
		color: var(--text-3);
	}
	.summary b {
		display: inline-block;
		color: var(--accent);
	}
	.buttons {
		display: flex;
		gap: 8px;
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
		.album-grid {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
		.thead,
		.tr {
			grid-template-columns: 44px 44px minmax(0, 1fr);
		}
		.col-artist {
			display: none;
		}
	}
	/* Phones: the summary and the buttons on their own rows. */
	@media (max-width: 600px) {
		.savebar {
			flex-wrap: wrap;
			justify-content: flex-end;
			padding: 10px 12px;
			border-radius: 22px;
		}
		.summary {
			flex: 1 1 100%;
			padding-left: 4px;
		}
		.hint {
			display: none;
		}
	}
</style>
