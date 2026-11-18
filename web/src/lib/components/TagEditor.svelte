<script lang="ts" module>
	import type { AlbumDetail } from '#lib/api.ts';
	import type { TagDraft as Kept } from '#lib/tag-form.ts';

	/** Edits in the editor: none, some not sent yet, or some being saved. */
	export type Draft = 'none' | 'draft' | 'saving';

	/**
	 * Edits whose save was refused because the album had changed elsewhere,
	 * kept to put on top of the album as it is now (see tag-form).
	 */
	export type TagDraft = Kept;

	/**
	 * Whether `detail` still has the draft's track `id`: under that id and at
	 * the same path, as an id freed by a track removed can name a new one.
	 */
	export function stillHere(draft: TagDraft, detail: AlbumDetail, id: number): boolean {
		return detail.tracks.some((t) => t.id === id && t.rel_path === draft.paths[id]);
	}

	/** How many tracks a draft edited that `detail` no longer has: those edits can't be put back. */
	export function lostTracks(draft: TagDraft, detail: AlbumDetail): number {
		return Object.keys(draft.edits).filter((id) => !stillHere(draft, detail, +id)).length;
	}
</script>

<script lang="ts">
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import Check from '@lucide/svelte/icons/check';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { onDestroy } from 'svelte';
	import { fly, slide } from 'svelte/transition';
	import { SvelteSet } from 'svelte/reactivity';
	import {
		api,
		ApiError,
		TagConflict,
		type AlbumTags,
		type ImportEdits,
		type TagWriteResult
	} from '#lib/api.ts';
	import { drafts } from '#lib/drafts.svelte.ts';
	import { setup } from '#lib/setup.svelte.ts';
	import { toast } from '#lib/toast.svelte.ts';
	import {
		ALBUM_CORE,
		CORE,
		MULTILINE,
		TRACK_CORE,
		changeCount,
		changed,
		changedElsewhere,
		edits,
		invalid,
		kindsIn,
		otherTags,
		setAll,
		shared,
		valueOf,
		withKept,
		type Values
	} from '#lib/tag-form.ts';
	import ShowAll from './ShowAll.svelte';

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

	/** The editor's own names for the fields up front. */
	const LABEL: Record<string, string> = {
		album: 'Album',
		album_artist: 'Album artist',
		date: 'Date',
		genre: 'Genre',
		track_number: '#',
		disc_number: 'Disc',
		title: 'Title',
		artist: 'Artist'
	};
	const label = (kind: string) => LABEL[kind] ?? setup.label(kind);
	const WIDE = new Set(['album', 'album_artist']);
	const NUMERIC = new Set(['track_number', 'disc_number', 'track_total', 'disc_total']);

	// The editor is made anew for each album version (see AlbumDetail).
	// svelte-ignore state_referenced_locally
	const tracks = detail.tracks;
	const ids = tracks.map((t) => t.id);
	const here = (id: number) => (kept ? stillHere(kept, detail, id) : true);

	let loading = $state(true);
	let failed = $state<string | null>(null);
	let base = $state<Values>({});
	let form = $state<Values>({});
	let other = $state<AlbumTags['tracks']>([]);
	/** Per (track, kind) changed elsewhere since the kept edits: what it was then. */
	let elsewhere = $state<Record<number, Record<string, string | null>>>({});
	/** Kinds added with "Add a tag", shown though no track has them yet. */
	const added = new SvelteSet<string>();
	/** Album-wide fields opened to edit each track's value. */
	const perTrack = new SvelteSet<string>();
	let saving = $state(false);

	async function load() {
		loading = true;
		failed = null;
		try {
			const [tags] = await Promise.all([api.albumTags(detail.id), setup.load()]);
			const values: Values = {};
			for (const id of ids) {
				const read = tags.tracks.find((t) => t.id === id)?.values ?? {};
				// Every field up front exists on every track, so inputs can bind to it.
				values[id] = { ...Object.fromEntries(CORE.map((k) => [k, ''])), ...read };
			}
			base = structuredClone(values);
			if (kept) {
				elsewhere = changedElsewhere(kept, values, here);
				form = withKept(structuredClone(values), kept, here);
				for (const kind of kindsIn(kept.edits)) if (!CORE.includes(kind)) added.add(kind);
				// A field kept edits made differ per track opens per track.
				for (const kind of ALBUM_CORE) if (shared(form, ids, kind) === null) perTrack.add(kind);
			} else form = structuredClone(values);
			other = tags.tracks;
		} catch (e) {
			failed = e instanceof ApiError ? e.message : 'The tags could not be read.';
		} finally {
			loading = false;
		}
	}
	load();

	const anyChanged = (kind: string) => ids.some((id) => changed(form, base, id, kind));
	const anyElsewhere = (kind: string) => ids.some((id) => elsewhere[id]?.[kind] !== undefined);
	const wrong = (id: number, kind: string) =>
		changed(form, base, id, kind) && invalid(kind, valueOf(form, id, kind));

	let changes = $derived(loading ? 0 : changeCount(form, base, ids));
	let errors = $derived(ids.some((id) => Object.keys(form[id] ?? {}).some((k) => wrong(id, k))));

	/** The other kinds the album has, or was given, in catalog order. */
	let extra = $derived.by(() => {
		const present = kindsIn(base, form);
		const all = setup.kinds.length ? setup.kinds.map((k) => k.id) : [...present].sort();
		return all.filter((k) => !CORE.includes(k) && (present.has(k) || added.has(k)));
	});
	let extraGroups = $derived(
		setup.groups
			.map((g) => ({
				...g,
				kinds: extra.filter((k) => setup.kinds.find((x) => x.id === k)?.group === g.id)
			}))
			.filter((g) => g.kinds.length)
	);
	let extraChanged = $derived(extra.filter(anyChanged).length);
	/** Kinds "Add a tag" offers: those not shown, that can be typed. */
	let addable = $derived(
		setup.groups
			.map((g) => ({
				...g,
				kinds: setup.kinds.filter(
					(k) =>
						k.group === g.id &&
						!CORE.includes(k.id) &&
						!extra.includes(k.id) &&
						!setup.notEditable.includes(k.id)
				)
			}))
			.filter((g) => g.kinds.length)
	);
	let others = $derived(otherTags(other));

	function addKind(kind: string) {
		if (!kind) return;
		added.add(kind);
		requestAnimationFrame(() =>
			document.querySelector<HTMLElement>(`[data-kind="${kind}"] .all`)?.focus()
		);
	}

	// So that signing out asks before dropping them.
	$effect(() => {
		drafts.tags = changes > 0;
		return () => (drafts.tags = false);
	});
	$effect(() => ondraft?.(saving ? 'saving' : changes > 0 ? 'draft' : 'none'));
	/** Whether this editor is still open: a refused save's edits are read from it, if so. */
	let open = true;
	onDestroy(() => {
		open = false;
		ondraft?.('none');
	});

	/** Values as a field's note gives them: one, or several when mixed. */
	function described(values: string[]): string {
		if (values.length > 1) return `mixed values (${values.map((v) => v || 'empty').join(', ')})`;
		return values[0] ? `“${values[0]}”` : 'empty';
	}
	/** What a field changed elsewhere says: what its tracks had, and have now. */
	function albumNote(kind: string): string | undefined {
		if (!kept || !anyElsewhere(kind)) return undefined;
		const was = [...new Set(ids.filter(here).map((id) => kept!.base[id]?.[kind] ?? ''))].sort();
		const now = [...new Set(ids.map((id) => valueOf(base, id, kind)))].sort();
		return `Changed elsewhere, from ${described(was)} to ${described(now)}`;
	}
	function trackNote(id: number, kind: string): string | undefined {
		const was = elsewhere[id]?.[kind];
		if (was === undefined) return undefined;
		const now = described([valueOf(base, id, kind)]);
		if (was === null) return `Added elsewhere: ${now}`;
		return `Changed elsewhere, from ${described([was])} to ${now}`;
	}

	/** What an album-wide field says when it shows no single value. */
	function placeholder(kind: string): string {
		const now = shared(form, ids, kind);
		if (now === '' && ids.some((id) => valueOf(base, id, kind) !== '') && anyChanged(kind))
			return 'Cleared on every track';
		if (now !== null) return '';
		return ids.some((id) => changed(form, base, id, kind))
			? 'Differs per track'
			: 'Mixed — leave empty to keep';
	}

	function revert() {
		form = structuredClone($state.snapshot(base));
		added.clear();
		onrevert?.();
	}

	/** The edits, kept to put back on the album as it is now. `sent`: the album they were made on. */
	function keep(sent: AlbumDetail): TagDraft {
		const paths = Object.fromEntries(sent.tracks.map((t) => [t.id, t.rel_path]));
		return { base: $state.snapshot(base), edits: edits(form, base, ids), paths };
	}

	async function save() {
		saving = true;
		// The album as saved: once answered, `detail` may be another's (the sheet
		// closed, and another album opened).
		const sent = detail;
		const id = sent.id;
		try {
			const result = await api.writeTags(id, {
				kinds: edits(form, base, ids),
				version: sent.version,
				base_tracks: sent.tracks.map(({ id, rel_path }) => ({ id, rel_path }))
			});
			toast(
				`Saved tags to ${result.files_written} ${result.files_written === 1 ? 'file' : 'files'}`,
				'success'
			);
			// Saved: no longer a draft, while the album is reloaded with them.
			base = structuredClone($state.snapshot(form));
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

{#snippet input(id: number | null, kind: string, aria: string)}
	{@const readonly = setup.notEditable.includes(kind)}
	{#if id === null}
		{@const value = shared(form, ids, kind) ?? ''}
		{#if MULTILINE.has(kind)}
			<textarea
				class="all"
				rows="3"
				{value}
				{readonly}
				title={albumNote(kind)}
				placeholder={placeholder(kind)}
				aria-label={aria}
				spellcheck="false"
				oninput={(e) => setAll(form, base, ids, kind, e.currentTarget.value)}></textarea>
		{:else}
			<input
				class="all"
				{value}
				{readonly}
				title={albumNote(kind)}
				placeholder={placeholder(kind)}
				aria-label={aria}
				inputmode={NUMERIC.has(kind) ? 'numeric' : undefined}
				spellcheck="false"
				oninput={(e) => setAll(form, base, ids, kind, e.currentTarget.value)}
			/>
		{/if}
	{:else if MULTILINE.has(kind)}
		<textarea
			class:changed={changed(form, base, id, kind)}
			class:elsewhere={elsewhere[id]?.[kind] !== undefined}
			class:invalid={wrong(id, kind)}
			title={trackNote(id, kind)}
			rows="3"
			{readonly}
			bind:value={form[id][kind]}
			aria-label={aria}
			spellcheck="false"></textarea>
	{:else}
		<input
			class:changed={changed(form, base, id, kind)}
			class:elsewhere={elsewhere[id]?.[kind] !== undefined}
			class:invalid={wrong(id, kind)}
			title={trackNote(id, kind)}
			{readonly}
			bind:value={form[id][kind]}
			aria-label={aria}
			inputmode={NUMERIC.has(kind) ? 'numeric' : undefined}
			spellcheck="false"
		/>
	{/if}
{/snippet}

{#snippet toggle(kind: string)}
	<button
		type="button"
		class="per-track"
		class:open={perTrack.has(kind)}
		aria-expanded={perTrack.has(kind)}
		title="Edit each track's value"
		onclick={() => (perTrack.has(kind) ? perTrack.delete(kind) : perTrack.add(kind))}
	>
		<ChevronRight size={13} /> Per track
	</button>
{/snippet}

{#snippet tracksOf(kind: string)}
	<div class="track-values" transition:slide={{ duration: 200 }}>
		{#each tracks as t, i (t.id)}
			<label class="track-value">
				<span class="which tabular" title={t.title}
					><b>{t.track_no ?? i + 1}</b> {t.title || t.rel_path}</span
				>
				{@render input(t.id, kind, `${label(kind)}, track ${i + 1}`)}
			</label>
		{/each}
	</div>
{/snippet}

<div class="editor">
	{#if loading}
		<p class="status"><span class="spin"><LoaderCircle size={15} /></span> Reading the tags…</p>
	{:else if failed}
		<p class="status failed">
			{failed}
			<button class="link" onclick={load}>Try again</button>
		</p>
	{:else}
		<section>
			<h3>Album</h3>
			<div class="album-grid">
				{#each ALBUM_CORE as kind (kind)}
					<div
						class="field"
						class:wide={WIDE.has(kind)}
						class:changed={anyChanged(kind)}
						class:elsewhere={anyElsewhere(kind)}
						class:invalid={ids.some((id) => wrong(id, kind))}
					>
						<span class="label"
							>{label(kind)}{#if anyChanged(kind)}<i class="dot" in:fly={{ y: -4 }}
								></i>{/if}{#if anyElsewhere(kind)}<i class="dot warn"></i>{/if}
							{@render toggle(kind)}</span
						>
						{@render input(null, kind, label(kind))}
					</div>
				{/each}
			</div>
			{#each ALBUM_CORE.filter((k) => perTrack.has(k)) as kind (kind)}
				<div class="per-track-block" transition:slide={{ duration: 200 }}>
					<h4>{label(kind)}, per track</h4>
					{@render tracksOf(kind)}
				</div>
			{/each}
		</section>

		<section>
			<h3>Tracks</h3>
			<div class="table" role="table" aria-label="Track tags">
				<div class="thead" role="row">
					{#each TRACK_CORE as kind (kind)}<span role="columnheader" class="col-{kind}"
							>{label(kind)}</span
						>{/each}
				</div>
				{#each tracks as t, i (t.id)}
					<div class="tr" role="row" style:--i={Math.min(i, 24)}>
						{#each TRACK_CORE as kind (kind)}
							<input
								class="cell col-{kind}"
								class:changed={changed(form, base, t.id, kind)}
								class:elsewhere={elsewhere[t.id]?.[kind] !== undefined}
								title={trackNote(t.id, kind)}
								class:invalid={wrong(t.id, kind)}
								bind:value={form[t.id][kind]}
								aria-label="{label(kind)}, track {i + 1}"
								inputmode={NUMERIC.has(kind) ? 'numeric' : undefined}
								spellcheck="false"
							/>
						{/each}
					</div>
				{/each}
			</div>
		</section>

		<ShowAll
			label="Show all tags"
			count={extra.length}
			badge={extraChanged ? `${extraChanged} changed` : undefined}
			initiallyOpen={extraChanged > 0}
		>
			<div class="all-tags">
				{#each extraGroups as g (g.id)}
					<div class="group">
						<h4>{g.label}</h4>
						{#each g.kinds as kind (kind)}
							<div
								class="row"
								data-kind={kind}
								class:changed={anyChanged(kind)}
								class:elsewhere={anyElsewhere(kind)}
								class:invalid={ids.some((id) => wrong(id, kind))}
							>
								<span class="label"
									>{label(kind)}{#if anyChanged(kind)}<i class="dot" in:fly={{ y: -4 }}
										></i>{/if}{#if anyElsewhere(kind)}<i class="dot warn"></i>{/if}</span
								>
								{@render input(null, kind, label(kind))}
								{#if tracks.length > 1}{@render toggle(kind)}{/if}
							</div>
							{#if perTrack.has(kind)}{@render tracksOf(kind)}{/if}
						{/each}
					</div>
				{/each}
				{#if addable.length}
					<label class="add">
						<span>Add a tag</span>
						<select
							onchange={(e) => {
								addKind(e.currentTarget.value);
								e.currentTarget.value = '';
							}}
						>
							<option value="">Choose a tag…</option>
							{#each addable as g (g.id)}
								<optgroup label={g.label}>
									{#each g.kinds as k (k.id)}<option value={k.id}>{k.label}</option>{/each}
								</optgroup>
							{/each}
						</select>
					</label>
				{/if}
				{#if others.length}
					<div class="group other">
						<h4>Other tags in the files</h4>
						<p class="note">
							coversync has no name for these: they're kept as they are, but can't be edited here.
						</p>
						<dl>
							{#each others as o (o.key)}
								<dt>{o.key}</dt>
								<dd>
									{#if o.value !== null}
										{o.value}
									{:else}
										<details>
											<summary>Differs per track</summary>
											<ul>
												{#each o.tracks as t (t.id)}
													{@const track = tracks.find((x) => x.id === t.id)}
													<li>
														<span class="which tabular"
															><b>{track?.track_no ?? '–'}</b>
															{track?.title ?? ''}</span
														>
														{t.value}
													</li>
												{/each}
											</ul>
										</details>
									{/if}
								</dd>
							{/each}
						</dl>
					</div>
				{/if}
			</div>
		</ShowAll>
	{/if}

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
					disabled={changes === 0 || errors || saving || loading}
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
					onclick={() => onimport(edits(form, base, ids))}
					disabled={errors || saving || loading}
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

	/* Each track's value of a field. */
	.per-track {
		display: inline-flex;
		align-items: center;
		gap: 3px;
		margin-left: auto;
		padding: 1px 6px 1px 2px;
		border-radius: 6px;
		font-size: 11.5px;
		font-weight: 550;
		color: var(--text-3);
		transition: color 140ms;
	}
	.per-track:hover {
		color: var(--text);
	}
	.per-track :global(svg) {
		transition: rotate 180ms var(--ease-out);
	}
	.per-track.open :global(svg) {
		rotate: 90deg;
	}
	.per-track-block {
		margin-top: 16px;
	}
	h4 {
		margin: 0 0 8px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--text-2);
	}
	.track-values {
		display: grid;
		gap: 6px;
		margin: 4px 0 12px;
	}
	.track-value {
		display: grid;
		grid-template-columns: minmax(0, 220px) minmax(0, 1fr);
		gap: 10px;
		align-items: center;
	}
	.which {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 13px;
		color: var(--text-2);
	}
	.which b {
		display: inline-block;
		min-width: 1.6em;
		color: var(--text-3);
		font-weight: 500;
	}
	textarea {
		width: 100%;
		padding: 8px 12px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface);
		outline: none;
		font: inherit;
		resize: vertical;
	}
	textarea:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.changed textarea,
	textarea.changed {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
	}
	.invalid textarea,
	textarea.invalid {
		border-color: var(--danger);
	}
	input[readonly],
	textarea[readonly] {
		color: var(--text-3);
	}

	/* Show all tags. */
	.all-tags {
		display: grid;
		gap: 22px;
	}
	.group {
		display: grid;
		gap: 8px;
	}
	.row {
		display: grid;
		grid-template-columns: minmax(0, 200px) minmax(0, 1fr) auto;
		gap: 10px;
		align-items: center;
	}
	.row .label {
		min-width: 0;
	}
	.add {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 13px;
		color: var(--text-2);
	}
	.add select {
		height: 36px;
		padding: 0 10px;
		border-radius: var(--radius-sm);
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text);
		font: inherit;
	}
	.other .note {
		margin: 0;
		font-size: 12.5px;
		color: var(--text-3);
	}
	.other dl {
		display: grid;
		grid-template-columns: minmax(0, 220px) minmax(0, 1fr);
		gap: 6px 12px;
		margin: 4px 0 0;
		font-size: 13px;
	}
	.other dt {
		font-family: var(--mono, ui-monospace, monospace);
		font-size: 12px;
		color: var(--text-2);
		overflow-wrap: anywhere;
	}
	.other dd {
		margin: 0;
		color: var(--text-2);
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}
	.other summary {
		cursor: pointer;
		color: var(--text-3);
	}
	.other ul {
		margin: 6px 0 0;
		padding: 0;
		list-style: none;
		display: grid;
		gap: 4px;
	}
	.status {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 8px 0;
		color: var(--text-2);
		font-size: 13.5px;
	}
	.status.failed {
		color: var(--warn-text);
	}
	.link {
		color: var(--accent);
		font-weight: 600;
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
		.row,
		.track-value,
		.other dl {
			grid-template-columns: minmax(0, 1fr);
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
