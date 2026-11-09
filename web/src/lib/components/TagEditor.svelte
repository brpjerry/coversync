<script lang="ts" module>
	import type { AlbumDetail } from '#lib/api.ts';

	/** Edits in the editor: none, some not sent yet, or some being saved. */
	export type Draft = 'none' | 'draft' | 'saving';

	export type AlbumField = 'album' | 'album_artist' | 'year' | 'genre';
	export type TrackField = 'track_no' | 'disc_no' | 'title' | 'artist';

	/** The editor's values for an album, as its details give them. */
	export interface TagValues {
		/** Album fields, empty where the tracks differ (mixed). */
		album: Record<AlbumField, string>;
		/** Each track's fields, by id. */
		tracks: Record<number, Record<TrackField, string>>;
		/** Each track's album fields as they are, mixed or not. */
		albumTracks: Record<number, Record<AlbumField, string>>;
	}

	/**
	 * Edits whose save was refused because the album had changed elsewhere,
	 * kept to put on top of the album as it is now.
	 */
	export interface TagDraft {
		/** The values the edits were made on. */
		base: TagValues;
		/** The album fields edited, with their values; '' clears one. */
		album: Partial<Record<AlbumField, string>>;
		/** Each track's edited fields, by id. */
		tracks: Record<number, Partial<Record<TrackField, string>>>;
		/** Where each track's file was. */
		paths: Record<number, string>;
	}

	/**
	 * Whether `detail` still has the draft's track `id`: under that id and at
	 * the same path, as an id freed by a track removed can name a new one.
	 */
	export function stillHere(draft: TagDraft, detail: AlbumDetail, id: number): boolean {
		return detail.tracks.some((t) => t.id === id && t.rel_path === draft.paths[id]);
	}

	/** How many tracks a draft edited that `detail` no longer has: those edits can't be put back. */
	export function lostTracks(draft: TagDraft, detail: AlbumDetail): number {
		return Object.keys(draft.tracks).filter((id) => !stillHere(draft, detail, +id)).length;
	}
</script>

<script lang="ts">
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { onDestroy } from 'svelte';
	import { fly } from 'svelte/transition';
	import {
		api,
		ApiError,
		TagConflict,
		type ImportEdits,
		type TagEdit,
		type TagField,
		type TagWriteResult,
		type Track
	} from '#lib/api.ts';
	import { drafts } from '#lib/drafts.svelte.ts';
	import { toast } from '#lib/toast.svelte.ts';

	interface Props {
		detail: AlbumDetail;
		/** Whether these files may be saved in place; Save is hidden otherwise. */
		allowWrites: boolean;
		/** A save, answered; `id` is the album's id as it was. */
		onsaved: (result: TagWriteResult, id: string) => void;
		oncancel: () => void;
		/** Inbox albums: import with the current edits applied to the Library copy. */
		onimport?: (edits: ImportEdits) => void;
		/** Hears what edits there are; a failed save turns them back into a draft. */
		ondraft?: (draft: Draft) => void;
		/**
		 * Edits kept from a refused save, put on top of `detail` (the album as it is
		 * now) when the editor opens. The fields changed since they were made are marked.
		 */
		kept?: TagDraft | null;
		/**
		 * A save was refused: the album changed elsewhere since `detail`. `current` is
		 * the album as it is now (null when it's gone), and `id` the album's id as it
		 * was. `draft` reads this editor's edits as they are then, or null once it has
		 * closed.
		 */
		onrefused?: (current: AlbumDetail | null, draft: () => TagDraft | null, id: string) => void;
		/** Revert was pressed: the edits, kept ones included, are dropped. */
		onrevert?: () => void;
	}
	let {
		detail,
		allowWrites,
		onsaved,
		oncancel,
		onimport,
		ondraft,
		kept = null,
		onrefused,
		onrevert
	}: Props = $props();

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

	/** The album's values as `detail` has them. */
	function initial(): TagValues {
		return {
			album: initialAlbum(),
			tracks: initialTracks(),
			albumTracks: Object.fromEntries(
				detail.tracks.map((t) => [
					t.id,
					Object.fromEntries(albumFields.map((f) => [f.key, value(t, f.key)]))
				])
			) as TagValues['albumTracks']
		};
	}

	/** `values` with the kept edits on top: every field edited, on tracks still here. */
	function withKept(values: TagValues): TagValues {
		if (!kept) return values;
		for (const f of albumFields) {
			const edit = kept.album[f.key];
			if (edit !== undefined) values.album[f.key] = edit;
		}
		for (const [id, edited] of Object.entries(kept.tracks)) {
			if (!stillHere(kept, detail, +id)) continue;
			for (const f of trackFields) {
				const edit = edited[f.key];
				if (edit !== undefined) values.tracks[+id][f.key] = edit;
			}
		}
		return values;
	}

	/**
	 * What changed elsewhere since the kept edits were made, with what it was.
	 * An album field's tracks had these values (one, or several when mixed); a
	 * track field had this value, or null on a track that has joined since.
	 */
	function changedElsewhere(now: TagValues) {
		const album: Partial<Record<AlbumField, string[]>> = {};
		const tracks: Record<number, Partial<Record<TrackField, string | null>>> = {};
		if (!kept) return { album, tracks };
		const was = kept.base;
		const here = (id: number) => stillHere(kept, detail, id);
		for (const f of albumFields) {
			const values = (all: TagValues['albumTracks']) =>
				[...new Set(Object.values(all).map((t) => t[f.key]))].sort();
			const before = values(was.albumTracks);
			// The tracks' values, not the field as shown: a mixed field shows empty
			// however its tracks differ.
			const changed =
				before.join('\0') !== values(now.albumTracks).join('\0') ||
				Object.entries(now.albumTracks).some(
					([id, v]) => here(+id) && was.albumTracks[+id]?.[f.key] !== v[f.key]
				);
			if (changed) album[f.key] = before;
		}
		for (const [id, values] of Object.entries(now.tracks)) {
			const old = here(+id) ? was.tracks[+id] : undefined;
			const fields: Partial<Record<TrackField, string | null>> = {};
			for (const f of trackFields)
				if (old?.[f.key] !== values[f.key]) fields[f.key] = old?.[f.key] ?? null;
			if (Object.keys(fields).length) tracks[+id] = fields;
		}
		return { album, tracks };
	}

	/** Values as a field's note gives them: one, or several when mixed. */
	function described(values: string[]): string {
		if (values.length > 1) return `mixed values (${values.map((v) => v || 'empty').join(', ')})`;
		return values[0] ? `“${values[0]}”` : 'empty';
	}
	/**
	 * What a field changed elsewhere says about it: what it was, and what it is
	 * now (`base`, the album as it is now, under any edits made here).
	 */
	function albumNote(k: AlbumField): string | undefined {
		const was = elsewhere.album[k];
		if (!was) return undefined;
		const now = [...new Set(Object.values(base.albumTracks).map((t) => t[k]))].sort();
		return `Changed elsewhere, from ${described(was)} to ${described(now)}`;
	}
	function trackNote(id: number, k: TrackField): string | undefined {
		const was = elsewhere.tracks[id]?.[k];
		if (was === undefined) return undefined;
		const now = described([base.tracks[id][k]]);
		if (was === null) return `Added elsewhere: ${now}`;
		return `Changed elsewhere, from ${described([was])} to ${now}`;
	}

	const mixed = Object.fromEntries(
		albumFields.map((f) => [f.key, shared(f.key) === null])
	) as Record<AlbumField, boolean>;
	let base = $state(initial());
	let form = $state(withKept(initial()));
	let saving = $state(false);
	const elsewhere = changedElsewhere(initial());
	/**
	 * Album fields a kept edit clears where the tracks now differ: empty there
	 * would otherwise mean "keep each track's", as for a mixed field left be.
	 */
	const clearing = (): Set<AlbumField> =>
		new Set(albumFields.filter((f) => kept?.album[f.key] === '' && mixed[f.key]).map((f) => f.key));
	let clears = $state(clearing());

	const albumChanged = (k: AlbumField) =>
		form.album[k] !== base.album[k] || (clears.has(k) && form.album[k] === '');
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
	// So that signing out asks before dropping them.
	$effect(() => {
		drafts.tags = changes > 0;
		return () => (drafts.tags = false);
	});
	let errors = $derived(
		albumFields.some((f) => invalid(f.key, form.album[f.key])) ||
			detail.tracks.some((t) => trackFields.some((f) => invalid(f.key, form.tracks[t.id][f.key])))
	);

	$effect(() => ondraft?.(saving ? 'saving' : changes > 0 ? 'draft' : 'none'));
	/** Whether this editor is still open: a refused save's edits are read from it, if so. */
	let open = true;
	onDestroy(() => {
		open = false;
		ondraft?.('none');
	});

	function revert() {
		form = structuredClone($state.snapshot(base));
		clears = new Set();
		onrevert?.();
	}

	/** The edits, kept to put back on the album as it is now. `sent`: the album they were made on. */
	function keep(sent: AlbumDetail): TagDraft {
		const album: TagDraft['album'] = {};
		for (const f of albumFields) if (albumChanged(f.key)) album[f.key] = form.album[f.key];
		const tracks: TagDraft['tracks'] = {};
		for (const t of sent.tracks) {
			const edit: Partial<Record<TrackField, string>> = {};
			for (const f of trackFields)
				if (trackChanged(t.id, f.key)) edit[f.key] = form.tracks[t.id][f.key];
			if (Object.keys(edit).length) tracks[t.id] = edit;
		}
		const paths = Object.fromEntries(sent.tracks.map((t) => [t.id, t.rel_path]));
		return { base: $state.snapshot(base), album, tracks, paths };
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
		// The album as saved: once answered, `detail` may be another's (the sheet
		// closed, and another album opened).
		const sent = detail;
		const id = sent.id;
		try {
			const result = await api.writeTags(id, {
				album,
				tracks,
				version: sent.version,
				base_tracks: sent.tracks.map(({ id, rel_path }) => ({ id, rel_path }))
			});
			toast(
				`Saved tags to ${result.files_written} ${result.files_written === 1 ? 'file' : 'files'}`,
				'success'
			);
			// Saved: no longer a draft, while the album is reloaded with them.
			base = structuredClone($state.snapshot(form));
			clears = new Set();
			onsaved(result, id);
		} catch (e) {
			if (e instanceof TagConflict) {
				toast(
					e.album
						? 'Not saved: this album was changed elsewhere'
						: sent.collection === 'inbox'
							? 'Not saved: this album is no longer in the Inbox. It may have been imported or removed elsewhere.'
							: 'Not saved: this album is no longer in the Library. It may have been removed elsewhere.',
					'error',
					8000
				);
				// Read when the refusal is shown, so edits typed till then are kept;
				// nothing once this editor has closed.
				onrefused?.(e.album, () => (open ? keep(sent) : null), id);
			} else toast(e instanceof ApiError ? e.message : 'Saving failed', 'error', 6000);
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
					class:elsewhere={f.key in elsewhere.album}
					class:invalid={invalid(f.key, form.album[f.key])}
				>
					<span class="label"
						>{f.label}{#if albumChanged(f.key)}<i class="dot" in:fly={{ y: -4 }}
							></i>{/if}{#if f.key in elsewhere.album}<i class="dot warn"></i>{/if}</span
					>
					<input
						title={albumNote(f.key)}
						bind:value={form.album[f.key]}
						placeholder={mixed[f.key]
							? clears.has(f.key)
								? 'Cleared on every track'
								: 'Mixed — leave empty to keep'
							: ''}
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
							class:elsewhere={f.key in (elsewhere.tracks[t.id] ?? {})}
							title={trackNote(t.id, f.key)}
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
	/* Changed elsewhere since kept edits were made (see TagDraft). */
	.elsewhere input,
	input.elsewhere {
		border-color: var(--warn-border);
		background: color-mix(in oklab, var(--warn-surface) 70%, var(--surface));
	}
	.changed input,
	input.changed {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
	}
	/* Edited here and changed elsewhere too: the edit is on top. */
	.changed.elsewhere input,
	input.changed.elsewhere {
		background: color-mix(in oklab, var(--warn-surface) 70%, var(--surface));
	}
	.dot.warn {
		background: var(--warn);
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
	/* Performance mode: no frost (app.css), so nearly solid. */
	:global(:root[data-performance]) .savebar {
		background: color-mix(in oklab, var(--bg-raised) 96%, transparent);
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
	/* Phones: a sheet along the bottom edge, summary and buttons on their own rows. */
	@media (max-width: 600px) {
		.savebar {
			bottom: 0;
			flex-wrap: wrap;
			justify-content: flex-end;
			/* Bleed past .editor's 16px and AlbumDetail's phone .body 4px to the sheet's edges. */
			margin: 0 -20px;
			padding: 10px 16px;
			border-radius: 0;
			border-width: 1px 0 0;
			box-shadow: 0 -12px 32px -16px rgb(0 0 0 / 0.5);
		}
		.summary {
			flex: 1 1 100%;
			padding-left: 4px;
		}
	}
</style>
