<script lang="ts" module>
	import type { AlbumDetail } from '#lib/api.ts';
	import type { TagDraft as Kept } from '#lib/tag-form.ts';

	/** Edits in the editor: none, some not sent yet, or some being saved. */
	export type Draft = 'none' | 'draft' | 'saving';

	/** What the editor shows open, kept by the sheet across a new editor (see `restore`). */
	export interface TagView {
		/** Show all tags is open. */
		allTags: boolean;
		/** Fields open per track. */
		perTrack: string[];
		/** Other tags in the files whose "Differs per track" is open. */
		otherTags: string[];
	}

	/**
	 * Edits whose save was refused because the album had changed elsewhere,
	 * kept to put on top of the album as it is now (see tag-form).
	 */
	export type TagDraft = Kept;
</script>

<script lang="ts">
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import Check from '@lucide/svelte/icons/check';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { onDestroy, untrack } from 'svelte';
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
		stillHere,
		withKept,
		type Values
	} from '#lib/tag-form.ts';
	import ShowAll from './ShowAll.svelte';

	interface Props {
		detail: AlbumDetail;
		/** A save, answered; `id` is the album's id as it was. */
		onsaved: (result: TagWriteResult, id: string) => void;
		/**
		 * Open as this view once the tags are read: what the editor before
		 * this one had open (one is made anew when the album changes
		 * elsewhere, or a save here is refused; after its own save, it reads
		 * the album in place).
		 */
		restore?: TagView | null;
		/** Hears what's open, as it changes. */
		onview?: (view: TagView) => void;
		/** Hears whether the tags are being read (the editor is short then). */
		onloading?: (loading: boolean) => void;
		oncancel: () => void;
		/**
		 * Inbox albums, which are read-only (no Save): import with the
		 * current edits applied to the Library copy.
		 */
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
		/**
		 * The album changed while its tags were read: load it as it is now,
		 * kept edits and all (Try again, then).
		 */
		onretry?: () => void;
	}
	let {
		detail,
		onsaved,
		oncancel,
		onimport,
		ondraft,
		kept = null,
		onrefused,
		onrevert,
		onretry,
		restore = null,
		onview,
		onloading
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
	/**
	 * The album's collection is picked from Library setup's (see
	 * CollectionsSetup), never typed; an Inbox album's in the import dialog.
	 */
	const COLLECTION = 'collection';
	const WIDE = new Set(['album', 'album_artist']);
	const NUMERIC = new Set(['track_number', 'disc_number', 'track_total', 'disc_total']);

	// The editor is made anew for each album version, except after its own
	// save, when it reads the album as saved in place (see AlbumDetail): the
	// tracks are the ones whose tags it shows.
	// svelte-ignore state_referenced_locally
	let tracks = $state(detail.tracks);
	let ids = $derived(tracks.map((t) => t.id));
	const here = (id: number) => (kept ? stillHere(kept, detail.tracks, id) : true);

	let loading = $state(true);
	let failed = $state<string | null>(null);
	/** The failure was the album changing while its tags were read. */
	let stale = $state(false);
	let base = $state<Values>({});
	let form = $state<Values>({});
	let other = $state<AlbumTags['tracks']>([]);
	/** Per (track, kind) changed elsewhere since the kept edits: what it was then. */
	let elsewhere = $state<Record<number, Record<string, string | null>>>({});
	/** Kinds added with "Add a tag", shown though no track has them yet. */
	const added = new SvelteSet<string>();
	/** Album-wide fields opened to edit each track's value. */
	const perTrack = new SvelteSet<string>();
	/** Show all tags is open. */
	let allTags = $state(false);
	const otherOpen = new SvelteSet<string>();
	let saving = $state(false);
	/**
	 * The album whose values the form holds: Save and Import wait while a
	 * newer one is read in place, as the form's tracks are still the older's.
	 */
	let showing = $state.raw<AlbumDetail | null>(null);
	/** Reads started: only the latest one's answer counts. */
	let reads = 0;
	/** The last read was in place: Try again reads in place too. */
	let quietRead = false;

	/**
	 * Read the tags of `detail`. `quiet`: the album as just saved here, read
	 * in place: what's shown stays until the new values replace it (nothing
	 * flashes), and edits typed meanwhile stay on top.
	 */
	async function load(quiet = false) {
		const read = ++reads;
		const latest = () => read === reads;
		if (!quiet) loading = true;
		failed = null;
		stale = false;
		quietRead = quiet;
		const shown = detail;
		try {
			const [tags] = await Promise.all([api.albumTags(shown.id), setup.load()]);
			// A later read (of a newer album, say) answers instead.
			if (!latest()) return;
			// The tag list always has kinds once it's loaded.
			if (!setup.kinds.length) {
				failed = 'The list of tags could not be read.';
				return;
			}
			// Read from the album as it is now: tracks and values from another
			// version than the one shown (and saved against) can't be paired.
			if (tags.version !== shown.version) {
				failed = 'This album changed while its tags were read. Try again to load it as it is now.';
				stale = true;
				return;
			}
			// Another, newer album came while these were read: it's read next.
			if (shown !== detail) return;
			const values: Values = {};
			for (const t of shown.tracks) {
				const read = tags.tracks.find((x) => x.id === t.id)?.values ?? {};
				// Every field up front exists on every track, so inputs can bind to it.
				values[t.id] = { ...Object.fromEntries(CORE.map((k) => [k, ''])), ...read };
			}
			if (quiet) {
				// What was typed since the save, on the album as saved.
				const typed = edits(form, base, ids);
				tracks = shown.tracks;
				base = structuredClone(values);
				form = structuredClone(values);
				let gone = 0;
				for (const [id, fields] of Object.entries(typed))
					if (form[+id]) Object.assign(form[+id], fields);
					else gone++;
				other = tags.tracks;
				showing = shown;
				// A track the save moved to another album (a new genre, say).
				if (gone)
					toast(
						`Edits typed meanwhile to ${gone === 1 ? 'a track' : `${gone} tracks`} the save moved to another album weren't kept`,
						'error',
						8000
					);
				return;
			}
			tracks = shown.tracks;
			base = structuredClone(values);
			if (kept) {
				elsewhere = changedElsewhere(kept, values, here);
				form = withKept(structuredClone(values), kept, here);
				for (const kind of kindsIn(kept.edits)) if (!CORE.includes(kind)) added.add(kind);
				// A field kept edits made differ per track opens per track.
				for (const kind of ALBUM_CORE) if (shared(form, ids, kind) === null) perTrack.add(kind);
			} else form = structuredClone(values);
			other = tags.tracks;
			// As the editor before this one had it (the album changed elsewhere, say); else
			// open where kept edits (or changes elsewhere) need looking at.
			if (restore) {
				allTags = restore.allTags || extraChanged > 0;
				for (const kind of restore.perTrack) perTrack.add(kind);
				for (const key of restore.otherTags) otherOpen.add(key);
			} else allTags = extraChanged > 0;
			showing = shown;
		} catch (e) {
			if (!latest()) return;
			failed = e instanceof ApiError ? e.message : 'The tags could not be read.';
			// Gone under this id: renamed (or removed) since; the sheet may
			// know it by its new one.
			if (e instanceof ApiError && e.status === 404) {
				failed =
					'This album was renamed or removed while its tags were read. Try again to load it as it is now.';
				stale = true;
			}
		} finally {
			if (latest()) loading = false;
		}
	}
	load();

	/**
	 * Try reading again: the same way, or, the album having changed since it
	 * was shown, on the album as it is now. Edits typed since a save then go
	 * on top of it, as a refused save's do.
	 */
	async function retry() {
		if (stale && changes > 0 && onrefused) {
			const id = detail.id;
			const rows = tracks;
			const current = await api.album(id).catch(() => null);
			onrefused(current, () => (open ? keep(rows) : null), id);
		} else if (stale && onretry) onretry();
		else load(quietRead);
	}
	// The album as saved here, handed over by the sheet: read in place.
	let reading = untrack(() => detail);
	$effect(() => {
		const next = detail;
		if (next === reading) return;
		reading = next;
		untrack(() => load(true));
	});

	$effect(() => onloading?.(loading));
	$effect(() => {
		if (loading || failed) return;
		onview?.({ allTags, perTrack: [...perTrack], otherTags: [...otherOpen] });
	});

	const anyChanged = (kind: string) => ids.some((id) => changed(form, base, id, kind));
	const anyElsewhere = (kind: string) => ids.some((id) => elsewhere[id]?.[kind] !== undefined);
	const wrong = (id: number, kind: string) =>
		changed(form, base, id, kind) && invalid(kind, valueOf(form, id, kind));

	let changes = $derived(loading ? 0 : changeCount(form, base, ids));
	let errors = $derived(ids.some((id) => Object.keys(form[id] ?? {}).some((k) => wrong(id, k))));

	/** The other kinds the album has, or was given, in catalog order. */
	let extra = $derived.by(() => {
		const present = kindsIn(base, form);
		// One changed elsewhere stays to show that, though it's empty now.
		const marked = new Set(Object.values(elsewhere).flatMap((kinds) => Object.keys(kinds)));
		const all = setup.kinds.length ? setup.kinds.map((k) => k.id) : [...present].sort();
		return all.filter(
			(k) =>
				!CORE.includes(k) && k !== COLLECTION && (present.has(k) || added.has(k) || marked.has(k))
		);
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
						k.id !== COLLECTION &&
						!extra.includes(k.id) &&
						!setup.notEditable.includes(k.id)
				)
			}))
			.filter((g) => g.kinds.length)
	);
	let others = $derived(otherTags(other));
	/**
	 * A Library album's collections to pick from: Library setup's, and any
	 * its tracks are in or are to be put in (one removed from setup since).
	 */
	let collections = $derived(
		onimport
			? []
			: [
					...new Set([
						...setup.saved.collections,
						...ids.flatMap((id) => [valueOf(base, id, COLLECTION), valueOf(form, id, COLLECTION)])
					])
				].filter(Boolean)
	);
	/**
	 * Every track goes in the collection; None ('') takes them out of theirs;
	 * null ("Differs per track") keeps each one's.
	 */
	function setCollection(value: string | null) {
		for (const id of ids) (form[id] ??= {})[COLLECTION] = value ?? valueOf(base, id, COLLECTION);
	}

	function addKind(kind: string) {
		if (!kind) return;
		added.add(kind);
		requestAnimationFrame(() =>
			document.querySelector<HTMLElement>(`[data-kind="${kind}"] .all`)?.focus()
		);
	}

	/** Kept edits not yet put back (still loading, or the load failed): still a draft. */
	let pending = $derived(kept !== null && (loading || failed !== null));
	// So that signing out asks before dropping them.
	$effect(() => {
		drafts.tags = changes > 0 || pending;
		return () => (drafts.tags = false);
	});
	$effect(() => ondraft?.(saving ? 'saving' : changes > 0 || pending ? 'draft' : 'none'));
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
		// The kept edits go, and with them what changed since they were made.
		elsewhere = {};
		onrevert?.();
	}

	/** The edits, kept to put back on the album as it is now. `rows`: the tracks they were made on. */
	function keep(rows: AlbumDetail['tracks']): TagDraft {
		const paths = Object.fromEntries(rows.map((t) => [t.id, t.rel_path]));
		return { base: $state.snapshot(base), edits: edits(form, base, ids), paths };
	}

	async function save() {
		saving = true;
		// The album as saved: once answered, `detail` may be another's (the sheet
		// closed, and another album opened).
		const sent = detail;
		const id = sent.id;
		// What's sent: edits typed while it's saved stay edits.
		const submitted = $state.snapshot(form);
		try {
			const result = await api.writeTags(id, {
				kinds: edits(submitted, base, ids),
				version: sent.version,
				base_tracks: sent.tracks.map(({ id, rel_path }) => ({ id, rel_path }))
			});
			const files = (n: number) => `${n} ${n === 1 ? 'file' : 'files'}`;
			toast(
				`Saved tags to ${files(result.files_written)}` +
					(result.files_moved
						? `, and moved ${files(result.files_moved)} to fit the folder structure`
						: ''),
				'success'
			);
			if (result.warning) toast(result.warning, 'error', 10000);
			// Saved: no longer a draft, while the album is reloaded with them;
			// what was marked as changed elsewhere is settled.
			base = submitted;
			elsewhere = {};
			onsaved(result, id);
		} catch (e) {
			if (e instanceof TagConflict) {
				toast(
					e.album
						? 'Not saved: this album was changed elsewhere'
						: 'Not saved: this album is no longer in the Library. It may have been removed elsewhere.',
					'error',
					8000
				);
				// Read when the refusal is shown, so edits typed till then are kept;
				// nothing once this editor has closed.
				onrefused?.(e.album, () => (open ? keep(sent.tracks) : null), id);
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
			<button class="link" onclick={retry}>Try again</button>
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
				{#if collections.length}
					{@const value = shared(form, ids, COLLECTION)}
					<div
						class="field"
						class:changed={anyChanged(COLLECTION)}
						class:elsewhere={anyElsewhere(COLLECTION)}
					>
						<span class="label"
							>Collection{#if anyChanged(COLLECTION)}<i class="dot" in:fly={{ y: -4 }}
								></i>{/if}{#if anyElsewhere(COLLECTION)}<i class="dot warn"></i>{/if}</span
						>
						<select
							bind:value={() => value, setCollection}
							title={albumNote(COLLECTION)}
							aria-label="Collection"
						>
							{#if value === null || shared(base, ids, COLLECTION) === null}<option value={null}
									>Differs per track</option
								>{/if}
							<option value="">None</option>
							{#each collections as c (c)}<option value={c}>{c}</option>{/each}
						</select>
					</div>
				{/if}
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
			count={extra.length + others.length}
			badge={extraChanged ? `${extraChanged} changed` : undefined}
			bind:open={allTags}
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
							Kept as they are when you save, but not editable here: tags coversync has no name for,
							ones held as data or with a description, and the files' other tag blocks. TRACKNUM, an
							old name for the track number, is dropped when a file with a track number is saved.
						</p>
						<dl>
							{#each others as o (o.key)}
								<dt>{o.key}</dt>
								<dd>
									{#if o.values !== null}
										{#each o.values as v, n (n)}<span class="value">{v}</span>{/each}
									{:else}
										<details
											open={otherOpen.has(o.key)}
											ontoggle={(e) =>
												e.currentTarget.open ? otherOpen.add(o.key) : otherOpen.delete(o.key)}
										>
											<summary>Differs per track</summary>
											<ul>
												{#each o.tracks as t (t.id)}
													{@const track = tracks.find((x) => x.id === t.id)}
													<li>
														<span class="which tabular"
															><b>{track?.track_no ?? '–'}</b>
															{track?.title ?? ''}</span
														>
														{#each t.values as v, n (n)}<span class="value">{v}</span>{/each}
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
				{changes === 1 ? 'change' : 'changes'}{onimport ? '' : ' not saved'}
				{#if onimport}<span class="hint">· written to the Library copy on import</span>{/if}
			{:else}
				No changes
			{/if}
		</span>
		<div class="buttons">
			<button class="btn" onclick={changes ? revert : oncancel}>
				{#if changes}<Undo2 size={15} /> Revert{:else if onimport}Cancel{:else}Done{/if}
			</button>
			{#if !onimport}
				<button
					class="btn primary"
					onclick={save}
					disabled={changes === 0 || errors || saving || loading || showing !== detail}
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
					disabled={errors || saving || loading || failed !== null || showing !== detail}
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
	input,
	.field select {
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
	.field select {
		color: var(--text);
		font: inherit;
	}
	input:hover,
	.field select:hover {
		border-color: var(--border-strong);
	}
	input:focus,
	.field select:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	/* Changed elsewhere since kept edits were made (see TagDraft). */
	.elsewhere input,
	.elsewhere select,
	input.elsewhere {
		border-color: var(--warn-border);
		background: color-mix(in oklab, var(--warn-surface) 70%, var(--surface));
	}
	.changed input,
	.changed select,
	input.changed {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
	}
	/* Edited here and changed elsewhere too: the edit is on top. */
	.changed.elsewhere input,
	.changed.elsewhere select,
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
	.col-track_number,
	.col-disc_number {
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
	/* A key held several times: each value on its own line. */
	.other .value {
		display: block;
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
