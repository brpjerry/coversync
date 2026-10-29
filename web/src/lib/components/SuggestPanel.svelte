<script lang="ts" module>
	/** What a suggestion row covers: one album-wide value, one track's, or one kind across tracks. */
	export type Scope = 'album' | 'track' | 'bundle';

	export interface Row {
		key: string;
		kind: string;
		scope: Scope;
		/** Tracks it applies to, with the value for each, as edited here. */
		ids: number[];
		values: Record<number, string>;
		/** MusicBrainz's value for each track, before any edits. */
		suggested: Record<number, string>;
		/** The value shown: the album-wide one, or an example for a bundle. */
		value: string;
		current: string | null;
		/** Album rows whose tracks have different current values. */
		mixed: boolean;
		/** Every track it covers has the tag empty. */
		fill: boolean;
		/** Shown up front, rather than under "More tags". */
		core: boolean;
		/** The import dialog has fields for it: accepting fills them in. */
		field: 'track' | 'album' | null;
		/** A romanized value: MusicBrainz's in the original script. */
		original: string | null;
		/** Every track it covers has `original` now: it only romanizes the tag. */
		romanizes: boolean;
	}

	/** The album's own art, to set MusicBrainz's beside. */
	export interface OriginalArt {
		thumb: string;
		full: string;
		width: number | null;
		height: number | null;
		/** e.g. "cover.jpg". */
		source: string;
	}

	/** MusicBrainz's cover, ticked for the import. */
	export interface ChosenArt extends MbArt {
		release: string;
	}
</script>

<script lang="ts">
	import { onDestroy } from 'svelte';
	import { fade, fly, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import { SvelteMap, SvelteSet } from 'svelte/reactivity';
	import Sparkles from '@lucide/svelte/icons/sparkles';
	import ArrowRight from '@lucide/svelte/icons/arrow-right';
	import ExternalLink from '@lucide/svelte/icons/external-link';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import Info from '@lucide/svelte/icons/info';
	import Languages from '@lucide/svelte/icons/languages';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import {
		api,
		ApiError,
		musicbrainzArtUrl,
		type IdentifyState,
		type Identification,
		type ImportEdits,
		type ImportTrack,
		type MbArt
	} from '#lib/api.ts';
	import ShowAll from './ShowAll.svelte';
	import ArtViewer, { type ViewedArt } from './ArtViewer.svelte';

	interface Props {
		albumId: string;
		tracks: ImportTrack[];
		/** Edits made before the dialog opened; they count as current values. */
		base: ImportEdits;
		label: (id: string) => string;
		/** Every kind, in catalog order. */
		order: string[];
		albumKinds: string[];
		coreKinds: string[];
		/** Kinds the dialog has fields for, per track and once per album. */
		fields: { track: string[]; album: string[] };
		/** A field's value; `id` null for an album field. */
		getField: (id: number | null, kind: string) => string;
		setField: (id: number | null, kind: string, value: string) => void;
		/** Accepted suggestions for tags without fields, and how many values are accepted in all. */
		onaccept: (edits: ImportEdits, count: number) => void;
		/** The album's own art; null if it has none. */
		original: OriginalArt | null;
		/** The release whose MusicBrainz cover the import uses, if any. */
		artChosen: string | null;
		/** `auto`: ticked by default rather than by hand. */
		onart: (art: ChosenArt | null, auto: boolean) => void;
	}
	let {
		albumId,
		tracks,
		base,
		label,
		order,
		albumKinds,
		coreKinds,
		fields,
		getField,
		setField,
		onaccept,
		original,
		artChosen,
		onart
	}: Props = $props();

	let lookup = $state<IdentifyState | null>(null);
	let result = $derived(lookup?.state === 'done' ? lookup.result : null);
	let rows = $state<Row[]>([]);
	const chosen = new SvelteSet<string>();
	/** Field values from before a suggestion filled them, to put back. */
	const restore = new SvelteMap<string, string>();

	const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
	let seq = 0;
	onDestroy(() => seq++);

	/** A release picked from the list; undefined for the best match. */
	let picked = $state<string>();
	/** Romanized or original script, picked here; undefined for Inbox setup's choice. */
	let romanize = $state<boolean>();

	async function identify(refresh = false) {
		const mine = ++seq;
		const failed = (e: unknown): IdentifyState => ({
			state: 'failed',
			error: e instanceof ApiError ? e.message : "coversyncd couldn't be reached."
		});
		let s: IdentifyState;
		try {
			s = await api.identify(albumId, picked, refresh, romanize);
			while (s.state === 'running') {
				if (mine !== seq) return;
				lookup = s;
				await sleep(300);
				s = await api.identifyStatus(albumId);
			}
		} catch (e) {
			s = failed(e);
		}
		if (mine !== seq) return;
		lookup = s;
		if (s.state === 'done') {
			rows = buildRows(s.result);
			applyDefaults(s.result);
			loadCover(s.result, mine);
		}
	}
	identify();

	/** MusicBrainz's cover for the release shown: `art` undefined while it downloads, null if none. */
	let cover = $state<{ release: string; art?: MbArt | null; error?: string } | null>(null);
	let artOn = $derived(!!cover?.art && artChosen === cover.release);

	async function loadCover(r: Identification, mine: number) {
		const release = r.release;
		cover = null;
		if (!release || (!release.front_art && !release.release_group)) return;
		cover = { release: release.id };
		try {
			const art = await api.musicbrainzArt(release.id, release.release_group, release.front_art);
			if (mine !== seq) return;
			cover = { release: release.id, art };
			// Like an empty tag: filled in when the release is surely the album.
			if (art && !original && complete) toggleArt(true, true);
		} catch (e) {
			if (mine !== seq) return;
			cover = {
				release: release.id,
				error: e instanceof ApiError ? e.message : "coversyncd couldn't be reached."
			};
		}
	}

	function toggleArt(on: boolean, auto = false) {
		onart(on && cover?.art ? { release: cover.release, ...cover.art } : null, auto);
	}

	/** The image the viewer shows first, while it's open. */
	let viewing = $state<number | null>(null);
	let artImages = $derived.by(() => {
		const out: ViewedArt[] = [];
		if (original) {
			const { full: url, width, height, source: note } = original;
			out.push({ label: 'Original', url, width, height, note });
		}
		if (cover?.art) {
			out.push({
				label: 'MusicBrainz',
				url: musicbrainzArtUrl(cover.release),
				width: cover.art.width,
				height: cover.art.height,
				note: cover.art.from === 'release' ? "the release's cover" : "the release group's cover"
			});
		}
		return out;
	});
	const pixels = (w: number | null, h: number | null) => (w && h ? `${w} × ${h}` : 'size unknown');
	/** The album art row: downloading, failed, or with an image (not when there's none). */
	let artRow = $derived(!!cover && cover.art !== null);
	let offered = $derived(rows.length + (cover?.art ? 1 : 0));
	/** MusicBrainz's cover has fewer pixels than the album's own. */
	let artSmaller = $derived(
		!!cover?.art &&
			!!original?.width &&
			!!original.height &&
			cover.art.width * cover.art.height < original.width * original.height
	);

	const num = (v: string) => /^\d+/.exec(v.trim())?.[0];
	/** Whether a value typed before the dialog already says what MusicBrainz does. */
	function same(kind: string, a: string, b: string) {
		if (/_(number|total)$/.test(kind)) return num(a) === num(b);
		return a.trim() === b.trim();
	}

	function buildRows(r: Identification): Row[] {
		type Entry = {
			id: number;
			current: string | null;
			value: string;
			original: string | null;
			romanizes: boolean;
		};
		const byKind = new Map<string, Entry[]>();
		for (const t of r.tracks) {
			for (const s of t.suggestions) {
				const typed = base[t.id]?.[s.kind];
				if (typed !== undefined && same(s.kind, typed, s.value)) continue;
				const current = (typed ?? s.current)?.trim() || null;
				const original = s.original ?? null;
				const romanizes =
					typed === undefined ? !!s.romanizes : !!original && same(s.kind, typed, original);
				const list = byKind.get(s.kind) ?? [];
				list.push({ id: t.id, current, value: s.value, original, romanizes });
				byKind.set(s.kind, list);
			}
		}
		const kinds = [...byKind.keys()].sort((a, b) => order.indexOf(a) - order.indexOf(b));
		const out: Row[] = [];
		for (const kind of kinds) {
			const entries = byKind.get(kind)!;
			const field = fields.track.includes(kind)
				? 'track'
				: fields.album.includes(kind)
					? 'album'
					: null;
			const uniform = entries.every((e) => e.value === entries[0].value);
			const core = coreKinds.includes(kind) || field !== null;
			const values = (list: Entry[]) => Object.fromEntries(list.map((e) => [e.id, e.value]));
			const script = (list: Entry[]) => {
				const originals = new Set(list.map((e) => e.original));
				return {
					original: originals.size === 1 ? list[0].original : null,
					romanizes: list.every((e) => e.romanizes)
				};
			};
			const currents = new Set(entries.map((e) => e.current ?? ''));
			// One row for the album when every track gets the same value: album
			// tags, and track tags making the same change everywhere (an artist).
			const shared =
				uniform &&
				(albumKinds.includes(kind) ? field !== 'track' : currents.size === 1 && entries.length > 1);
			if (shared) {
				out.push({
					key: `album:${kind}`,
					kind,
					scope: 'album',
					ids: entries.map((e) => e.id),
					values: values(entries),
					suggested: values(entries),
					value: entries[0].value,
					current: currents.size === 1 ? entries[0].current : null,
					mixed: currents.size > 1,
					fill: entries.every((e) => !e.current),
					core,
					// One album field holds one value for every track.
					field: field === 'album' ? (entries.length === tracks.length ? 'album' : null) : field,
					...script(entries)
				});
			} else if (core) {
				for (const e of entries) {
					out.push({
						key: `${e.id}:${kind}`,
						kind,
						scope: 'track',
						ids: [e.id],
						values: values([e]),
						suggested: values([e]),
						value: e.value,
						current: e.current,
						mixed: false,
						fill: !e.current,
						core,
						field: field === 'track' ? 'track' : null,
						...script([e])
					});
				}
			} else {
				for (const fill of [true, false]) {
					const part = entries.filter((e) => !e.current === fill);
					if (!part.length) continue;
					out.push({
						key: `bundle:${kind}:${fill}`,
						kind,
						scope: 'bundle',
						ids: part.map((e) => e.id),
						values: values(part),
						suggested: values(part),
						value: part[0].value,
						current: null,
						mixed: false,
						fill,
						core,
						field: null,
						...script(part)
					});
				}
			}
		}
		return out;
	}

	function isOn(row: Row) {
		if (row.ids.some((id) => !row.values[id].trim())) return false;
		if (row.field === 'track') {
			return row.ids.every((id) => getField(id, row.kind).trim() === row.values[id].trim());
		}
		if (row.field === 'album') return getField(null, row.kind).trim() === row.value.trim();
		return chosen.has(row.key);
	}

	function toggle(row: Row, on: boolean) {
		if (row.field) {
			const ids = row.field === 'album' ? [null] : row.ids;
			for (const id of ids) {
				const key = `${id}:${row.kind}`;
				if (on) {
					restore.set(key, getField(id, row.kind));
					setField(id, row.kind, id === null ? row.value : row.values[id]);
				} else {
					setField(id, row.kind, restore.get(key) ?? '');
					restore.delete(key);
				}
			}
		} else if (on) {
			chosen.add(row.key);
		} else {
			chosen.delete(row.key);
		}
	}

	/**
	 * Put new values in a row. Typing one ticks the row and clearing one
	 * unticks it; putting MusicBrainz's back (`tick` false) leaves it as it was.
	 */
	function update(row: Row, values: Record<number, string>, tick: boolean) {
		const was = isOn(row);
		Object.assign(row.values, values);
		row.value = row.values[row.ids[0]];
		const want = tick ? row.ids.every((id) => row.values[id].trim()) : was;
		if (want !== was) {
			toggle(row, want);
		} else if (was && row.field) {
			for (const id of row.field === 'album' ? [null] : row.ids) {
				setField(id, row.kind, id === null ? row.value : row.values[id]);
			}
		}
	}
	const typed = (ids: number[], value: string) => Object.fromEntries(ids.map((id) => [id, value]));
	const suggestedFor = (row: Row, ids: number[]) =>
		Object.fromEntries(ids.map((id) => [id, row.suggested[id]]));
	const edited = (row: Row, ids: number[]) =>
		ids.some((id) => row.values[id] !== row.suggested[id]);

	/** Bundles with a different value per track, opened to edit each one. */
	const opened = new SvelteSet<string>();

	/**
	 * A release with exactly the album's tracks, each paired: fill empty tags,
	 * and romanize tags in the original script. Anything else (a box set
	 * holding these recordings, say) starts unticked.
	 */
	let complete = $derived(
		!!result?.release &&
			result.release.track_count === tracks.length &&
			result.tracks.every((t) => t.via)
	);
	function applyDefaults(r: Identification) {
		const whole =
			!!r.release && r.release.track_count === tracks.length && r.tracks.every((t) => t.via);
		if (!whole) return;
		for (const row of rows) {
			// Never over what was typed in a field.
			const untouched = (v: string) => !v.trim() || (row.romanizes && v.trim() === row.current);
			const free =
				row.field === 'album'
					? untouched(getField(null, row.kind))
					: row.field === 'track'
						? row.ids.every((id) => untouched(getField(id, row.kind)))
						: true;
			if ((row.fill || row.romanizes) && free) toggle(row, true);
		}
	}

	/**
	 * Tick or untick every row, the album art's included; a cover smaller
	 * than the album's own is left as it is rather than ticked.
	 */
	function setAll(pick: (row: { fill: boolean }) => boolean) {
		for (const row of rows) {
			const want = pick(row);
			if (want !== isOn(row)) toggle(row, want);
		}
		const want = pick({ fill: !original });
		if (cover?.art && want !== artOn && !(want && artSmaller)) toggleArt(want);
	}

	/** Look again, after taking back what this lookup's suggestions filled in. */
	function relook() {
		setAll(() => false);
		restore.clear();
		opened.clear();
		rows = [];
		cover = null;
		identify();
	}
	function switchRelease(id: string) {
		picked = id;
		relook();
	}
	function setScript(romanized: boolean) {
		if (result?.romanization?.applied === romanized) return;
		romanize = romanized;
		relook();
	}

	let on = $derived(rows.filter(isOn));
	let accepted = $derived(on.reduce((n, r) => n + r.ids.length, 0));
	$effect(() => {
		const out: ImportEdits = {};
		for (const row of on) {
			if (row.field) continue;
			for (const id of row.ids) (out[id] ??= {})[row.kind] = row.values[id].trim();
		}
		onaccept(out, accepted);
	});

	let albumRows = $derived(rows.filter((r) => r.core && r.scope === 'album'));
	let trackGroups = $derived(
		tracks
			.map((t) => ({
				track: t,
				match: result?.tracks.find((x) => x.id === t.id),
				rows: rows.filter((r) => r.core && r.scope === 'track' && r.ids[0] === t.id)
			}))
			.filter((g) => g.rows.length)
	);
	let moreRows = $derived(rows.filter((r) => !r.core));
	let moreOn = $derived(moreRows.filter(isOn).length);
	let unpaired = $derived(
		result?.release ? tracks.filter((t) => !result.tracks.find((x) => x.id === t.id)?.via) : []
	);

	/** Identifiers read better in a fixed-width face. */
	const ID_KINDS = /^(mb_|acoustid|isrc|barcode)/;

	/** Which of the album's tracks a row covers, for the right-hand column. */
	function scopeText(row: Row) {
		if (row.scope === 'track') return undefined;
		const n = tracks.length;
		return row.ids.length === n ? `all ${n} tracks` : `${row.ids.length} of ${n} tracks`;
	}

	const STAGES: Record<string, string> = {
		reading: 'Reading the tags',
		fingerprinting: 'Fingerprinting the audio',
		acoustid: 'Asking AcoustID',
		searching: 'Searching MusicBrainz by title',
		release: 'Fetching the release from MusicBrainz',
		romanizing: 'Looking for romanized titles and names'
	};

	/** Where the romanized values came from, in a sentence or two. */
	let scriptNote = $derived.by(() => {
		const r = result?.romanization;
		if (!r) return '';
		if (!r.applied) return 'Titles and names as the release has them, in its own script.';
		const bits: string[] = [];
		if (r.titles) {
			bits.push(
				r.tracklist
					? `${r.tracklist.translated ? 'English' : 'Romanized'} titles from “${r.tracklist.title}”`
					: 'the album title from a MusicBrainz alias'
			);
		}
		if (r.names) {
			bits.push(
				`artist names from aliases and sort names${r.family_first ? ', family name first' : ''}`
			);
		}
		const done = bits.length
			? `${bits.join(', ').replace(/^./, (c) => c.toUpperCase())}.`
			: 'MusicBrainz has no romanized titles or names for this release.';
		const left = r.left
			? ` ${r.left} ${r.left === 1 ? 'stays' : 'stay'} in the original script, with nothing Latin on MusicBrainz.`
			: '';
		return done + left;
	});

	let how = $derived.by(() => {
		if (!result) return '';
		if (result.method === 'chosen') return 'Picked by you';
		if (result.method === 'tags') return 'Found by title and artist';
		return `Identified by audio: AcoustID recognized ${result.recognized} of ${tracks.length} ${tracks.length === 1 ? 'track' : 'tracks'}`;
	});

	function candidateText(c: Identification['candidates'][number]) {
		const bits = [
			c.title,
			c.artist,
			c.date?.slice(0, 4),
			c.country,
			c.format,
			c.track_count != null ? `${c.track_count} ${c.track_count === 1 ? 'track' : 'tracks'}` : null,
			c.matched ? `${c.matched} recognized` : null
		];
		return bits.filter(Boolean).join(' · ');
	}
</script>

{#snippet reset(row: Row, ids: number[], name: string)}
	{#if edited(row, ids)}
		<button
			class="reset"
			title="Back to MusicBrainz's value"
			aria-label="Put back MusicBrainz's {name}"
			onclick={() => update(row, suggestedFor(row, ids), false)}
			transition:fade={{ duration: 120 }}><Undo2 size={13} /></button
		>
	{/if}
{/snippet}

<!-- The value `ids` get, to edit in place; one input sets it for all of them. -->
{#snippet editor(row: Row, ids: number[], name: string)}
	{@const value = row.values[ids[0]]}
	<span class="now grow" class:mono={ID_KINDS.test(row.kind)} data-value={value}>
		<input
			{value}
			size="1"
			placeholder={row.suggested[ids[0]]}
			aria-label={name}
			title={row.original ? `${value} (${row.original})` : value}
			spellcheck="false"
			oninput={(e) => update(row, typed(ids, e.currentTarget.value), true)}
			onchange={(e) => {
				if (e.currentTarget.value.trim()) return;
				update(row, suggestedFor(row, ids), false);
				// Without an input event first the state never saw it empty, so set the box too.
				e.currentTarget.value = row.values[ids[0]];
			}}
		/>
	</span>
	{@render reset(row, ids, name)}
{/snippet}

<!-- The album's own art beside MusicBrainz's; either opens full size. -->
{#snippet artSuggestion()}
	{@const c = cover!}
	<label class="sugg" class:on={artOn} class:change={!!original}>
		<input
			type="checkbox"
			checked={artOn}
			disabled={!c.art}
			onchange={(e) => toggleArt(e.currentTarget.checked)}
		/>
		<span class="kind">Album art</span>
		<span class="diff">
			{#if original}
				<button class="thumb" onclick={() => (viewing = 0)} title="Show the album's art full size">
					<img src={original.thumb} alt="The album's art" />
				</button>
				<span class="was tabular">{pixels(original.width, original.height)}</span>
			{:else}
				<span class="was empty">none</span>
			{/if}
			<ArrowRight size={13} />
			{#if c.art}
				<button
					class="thumb"
					onclick={() => (viewing = original ? 1 : 0)}
					title="Show MusicBrainz's cover full size"
				>
					<img src={musicbrainzArtUrl(c.release)} alt="MusicBrainz's cover" />
				</button>
				<span class="now tabular">{pixels(c.art.width, c.art.height)}</span>
			{:else if c.error}
				<span class="art-error">{c.error}</span>
			{:else}
				<span class="fetching"
					><span class="spin"><LoaderCircle size={13} /></span> Fetching from the Cover Art Archive…</span
				>
			{/if}
		</span>
		{#if c.art?.from === 'release_group'}<span class="scope">the release group's</span>{/if}
	</label>
{/snippet}

{#snippet suggestion(row: Row, note?: string)}
	{@const checked = isOn(row)}
	{@const others = new Set(Object.values(row.suggested)).size - 1}
	{@const open = opened.has(row.key)}
	<label class="sugg" class:on={checked} class:change={!row.fill}>
		<input type="checkbox" {checked} onchange={(e) => toggle(row, e.currentTarget.checked)} />
		<span class="kind">{label(row.kind)}</span>
		<span class="diff">
			{#if row.scope === 'bundle'}
				<span class="was" class:empty={row.fill}>{row.fill ? 'empty' : 'differs'}</span>
				<ArrowRight size={13} />
				{#if others > 0}
					<span class="now" class:mono={ID_KINDS.test(row.kind)} title={row.value}>{row.value}</span
					>
					<button
						class="others tabular"
						aria-expanded={open}
						title={open ? 'Hide the value for each track' : 'Edit the value for each track'}
						onclick={() => (open ? opened.delete(row.key) : opened.add(row.key))}
						>+{others} more <span class="chev" class:open><ChevronDown size={11} /></span></button
					>
					{@render reset(row, row.ids, `${label(row.kind)} for every track`)}
				{:else}
					{@render editor(row, row.ids, label(row.kind))}
				{/if}
			{:else}
				{#if row.fill}
					<span class="was empty">empty</span>
				{:else}
					<s class="was" title={row.current ?? ''}>{row.mixed ? 'varies' : row.current}</s>
				{/if}
				<ArrowRight size={13} />
				{#if row.original}<span class="roman" title="Romanized from {row.original}"
						><Languages size={12} /></span
					>{/if}
				{@render editor(row, row.ids, label(row.kind))}
			{/if}
		</span>
		{#if note}<span class="scope">{note}</span>{/if}
	</label>
	{#if others > 0 && open}
		<div
			class="per-track"
			class:on={checked}
			class:change={!row.fill}
			transition:slide={{ duration: 220, easing: cubicOut }}
		>
			{#each row.ids as id (id)}
				{@const t = tracks.find((x) => x.id === id)}
				{@const name = t?.title || t?.file_name || ''}
				<div class="track-value">
					<span class="pos tabular">{result?.tracks.find((x) => x.id === id)?.position ?? '–'}</span
					>
					<span class="track-title" title={name}>{name}</span>
					<span class="diff">{@render editor(row, [id], `${label(row.kind)}, ${name}`)}</span>
				</div>
			{/each}
		</div>
	{/if}
{/snippet}

<section class="suggest" aria-labelledby="suggest-title" aria-busy={lookup?.state === 'running'}>
	<header class="suggest-head">
		<span class="badge" class:busy={!lookup || lookup.state === 'running'}
			><Sparkles size={16} /></span
		>
		<div class="head-main">
			<h3 id="suggest-title">Suggestions from MusicBrainz</h3>
			{#if !lookup || lookup.state === 'running'}
				{@const s = lookup?.state === 'running' ? lookup : null}
				<p class="stage">
					{#key s?.stage}<span in:fade={{ duration: 160 }}
							>{STAGES[s?.stage ?? 'reading']}{#if s?.stage === 'fingerprinting' && s.total}
								· {s.done} of {s.total}{/if}…</span
						>{/key}
				</p>
				<span class="progress" class:known={s?.stage === 'fingerprinting' && !!s.total}
					><span
						style:transform={s?.stage === 'fingerprinting' && s.total
							? `scaleX(${s.done / s.total})`
							: undefined}
					></span></span
				>
			{:else if lookup.state === 'failed'}
				<p class="stage error"><TriangleAlert size={14} /> {lookup.error}</p>
			{:else if result?.release}
				{@const r = result.release}
				<p class="match" in:fade={{ duration: 200 }}>
					<b>{r.title}</b>
					<span>{r.artist}</span>
					{#if r.disambiguation}<span class="dim">({r.disambiguation})</span>{/if}
				</p>
				<p class="facts" in:fade={{ duration: 200 }}>
					{[
						r.date,
						r.country,
						r.format,
						`${r.track_count} ${r.track_count === 1 ? 'track' : 'tracks'}`
					]
						.filter(Boolean)
						.join(' · ')}
					<span class="how">{how}</span>
				</p>
			{:else}
				<p class="stage">MusicBrainz has nothing for this album.</p>
			{/if}
		</div>
		<div class="head-actions">
			{#if lookup?.state === 'failed'}
				<button class="btn sm" onclick={() => identify(true)}>
					<RefreshCw size={14} /> Retry
				</button>
			{:else if result?.release}
				<a
					class="btn sm"
					href="https://musicbrainz.org/release/{result.release.id}"
					target="_blank"
					rel="noreferrer"
					title="Open the release on MusicBrainz">MusicBrainz <ExternalLink size={13} /></a
				>
			{/if}
		</div>
	</header>

	{#if result}
		{#if result.candidates.length > 1}
			<label class="pick" in:fade={{ duration: 200 }}>
				<span>Release</span>
				<select
					value={result.release?.id}
					onchange={(e) => switchRelease(e.currentTarget.value)}
					aria-label="Release to take tags from"
				>
					{#each result.candidates as c (c.id)}
						<option value={c.id}>{candidateText(c)}</option>
					{/each}
				</select>
			</label>
		{/if}
		{#if result.romanization}
			<div class="script" in:fade={{ duration: 200 }}>
				<div class="segmented" role="radiogroup" aria-label="Script for titles and names">
					<button
						role="radio"
						aria-checked={result.romanization.applied}
						class:on={result.romanization.applied}
						onclick={() => setScript(true)}
					>
						<Languages size={13} /> Romanized
					</button>
					<button
						role="radio"
						aria-checked={!result.romanization.applied}
						class:on={!result.romanization.applied}
						onclick={() => setScript(false)}
					>
						Original script
					</button>
				</div>
				<span class="script-note">{scriptNote}</span>
			</div>
		{/if}
		{#if result.release && !complete}
			<p class="note warn" transition:slide={{ duration: 200 }}>
				<TriangleAlert size={14} />
				<span>
					{#if result.release.track_count !== tracks.length}
						This release has {result.release.track_count}
						{result.release.track_count === 1 ? 'track' : 'tracks'}; the album has {tracks.length}.
						It may be a single, compilation or box set that includes these recordings, so nothing is
						ticked. Pick another release above if one fits better.
					{:else}
						{unpaired.length} of the album's tracks couldn't be paired with this release's, so nothing
						is ticked.
					{/if}
				</span>
			</p>
		{/if}
		{#each result.notes as note (note)}
			<p class="note"><Info size={14} /> <span>{note}</span></p>
		{/each}
		{#if unpaired.length && unpaired.length < tracks.length}
			<p class="note">
				<Info size={14} />
				<span
					>Not on this release, so only album-wide tags are suggested for {unpaired
						.map((t) => t.title || t.file_name)
						.join(', ')}.</span
				>
			</p>
		{/if}

		{#if rows.length || artRow}
			<div class="toolbar" in:fly={{ y: 6, duration: 240, easing: cubicOut }}>
				<span class="tabular"
					><b>{on.length + (artOn ? 1 : 0)}</b> of {offered}
					{offered === 1 ? 'suggestion' : 'suggestions'} ticked</span
				>
				<button class="link" onclick={() => setAll(() => true)}>Tick all</button>
				<button class="link" onclick={() => setAll((r) => r.fill)}>Only empty tags</button>
				<button class="link" onclick={() => setAll(() => false)}>None</button>
			</div>

			{#if albumRows.length || artRow}
				<div class="group">
					<h4>Album-wide</h4>
					{#if artRow}{@render artSuggestion()}{/if}
					{#each albumRows as row (row.key)}
						{@render suggestion(row, scopeText(row))}
					{/each}
				</div>
			{/if}

			{#if trackGroups.length}
				<div class="group">
					<h4>Tracks</h4>
					{#each trackGroups as g (g.track.id)}
						<div class="track">
							<span class="track-head">
								<span class="pos tabular">{g.match?.position ?? '–'}</span>
								<span class="track-title">{g.track.title || g.track.file_name}</span>
								{#if g.match?.via === 'position'}<span class="via">paired by number</span
									>{:else if g.match?.via === 'title'}<span class="via">paired by title</span>{/if}
							</span>
							{#each g.rows as row (row.key)}
								{@render suggestion(row)}
							{/each}
						</div>
					{/each}
				</div>
			{/if}

			{#if moreRows.length}
				<ShowAll
					label="Show all suggestions"
					count={moreRows.length}
					badge={moreOn ? `${moreOn} ticked` : undefined}
				>
					<div class="group more">
						{#each moreRows as row (row.key)}
							{@render suggestion(row, scopeText(row))}
						{/each}
					</div>
				</ShowAll>
			{/if}
		{/if}
		{#if !rows.length && result.release}
			<p class="agree" in:fade={{ duration: 200 }}>
				<CircleCheck size={15} /> Every tag already matches MusicBrainz.
			</p>
		{/if}
	{/if}
	{#if viewing !== null && artImages.length}
		<ArtViewer images={artImages} start={viewing} onclose={() => (viewing = null)} />
	{/if}
</section>

<style>
	.suggest {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		min-width: 0;
		gap: 10px;
		padding: 14px 16px 16px;
		border-radius: 16px;
		border: 1px solid color-mix(in oklab, var(--accent) 28%, var(--border));
		background: color-mix(in oklab, var(--accent) 6%, var(--surface));
	}
	.suggest-head {
		display: flex;
		align-items: flex-start;
		gap: 12px;
	}
	.badge {
		flex: none;
		display: grid;
		place-items: center;
		width: 34px;
		height: 34px;
		border-radius: 10px;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.badge.busy :global(svg) {
		animation: twinkle 1.4s ease-in-out infinite;
	}
	@keyframes twinkle {
		50% {
			opacity: 0.45;
			transform: scale(0.85) rotate(12deg);
		}
	}
	.head-main {
		flex: 1;
		min-width: 0;
		display: grid;
		gap: 2px;
	}
	h3 {
		margin: 0;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	.stage,
	.match,
	.facts {
		margin: 0;
		font-size: 13.5px;
		color: var(--text-2);
	}
	.stage.error {
		display: flex;
		gap: 6px;
		align-items: flex-start;
		color: var(--danger);
	}
	.stage.error :global(svg) {
		flex: none;
		margin-top: 3px;
	}
	.match {
		display: flex;
		flex-wrap: wrap;
		overflow-wrap: anywhere;
		gap: 0 8px;
		align-items: baseline;
	}
	.match b {
		font-size: 15px;
		color: var(--text);
	}
	.dim,
	.facts {
		color: var(--text-3);
		font-size: 12.5px;
	}
	.how {
		display: block;
		color: var(--accent);
	}
	.progress {
		display: block;
		height: 3px;
		margin-top: 6px;
		border-radius: 999px;
		overflow: hidden;
		background: color-mix(in oklab, var(--accent) 15%, transparent);
	}
	.progress span {
		display: block;
		height: 100%;
		width: 40%;
		border-radius: inherit;
		background: var(--accent);
		animation: sweep 1.1s var(--ease-out) infinite;
	}
	.progress.known span {
		width: 100%;
		animation: none;
		transform-origin: left;
		transition: transform 300ms var(--ease-out);
	}
	@keyframes sweep {
		from {
			transform: translateX(-100%);
		}
		to {
			transform: translateX(250%);
		}
	}
	.head-actions {
		flex: none;
		display: flex;
		gap: 6px;
	}
	.head-actions .btn {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 30px;
		padding: 0 11px;
		font-size: 12.5px;
		text-decoration: none;
	}
	.pick {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 12.5px;
		color: var(--text-3);
	}
	.pick select {
		/* A select is as wide as its longest option unless told otherwise. */
		flex: 1 1 0;
		width: 0;
		min-width: 0;
		text-overflow: ellipsis;
		height: 32px;
		padding: 0 10px;
		border-radius: 9px;
		border: 1px solid var(--border-strong);
		background: var(--surface);
		color: var(--text);
		font: inherit;
		font-size: 13px;
	}
	.note {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		margin: 0;
		font-size: 12.5px;
		color: var(--text-2);
	}
	.note :global(svg) {
		flex: none;
		margin-top: 2px;
		color: var(--text-3);
	}
	.note.warn {
		color: color-mix(in oklab, hsl(40 90% 70%) 85%, var(--text));
	}
	.note.warn :global(svg) {
		color: hsl(40 90% 62%);
	}
	.toolbar {
		display: flex;
		align-items: center;
		gap: 14px;
		flex-wrap: wrap;
		padding-top: 4px;
		font-size: 12.5px;
		color: var(--text-3);
	}
	.toolbar b {
		color: var(--text);
	}
	.toolbar span {
		margin-right: auto;
	}
	.link {
		color: var(--accent);
		font-size: 12.5px;
	}
	.link:hover {
		text-decoration: underline;
	}
	.group {
		display: grid;
		gap: 2px;
	}
	.group.more {
		padding-top: 6px;
	}
	h4 {
		margin: 6px 0 4px;
		font-size: 11.5px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	.track {
		display: grid;
		gap: 2px;
		padding: 4px 0;
	}
	.track + .track {
		border-top: 1px solid var(--border);
	}
	.track-head {
		display: flex;
		align-items: baseline;
		gap: 8px;
		padding: 2px 8px;
		font-size: 13px;
		font-weight: 580;
		min-width: 0;
	}
	.pos {
		flex: none;
		min-width: 22px;
		color: var(--text-3);
		font-weight: 500;
	}
	.track-title {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.via {
		flex: none;
		font-size: 11.5px;
		font-weight: 500;
		color: var(--text-3);
	}
	.sugg {
		display: grid;
		grid-template-columns: auto minmax(110px, 170px) minmax(0, 1fr) auto;
		align-items: center;
		gap: 10px;
		padding: 5px 8px;
		border-radius: 9px;
		font-size: 13px;
		cursor: pointer;
		transition: background 140ms;
	}
	.sugg:hover {
		background: color-mix(in oklab, var(--accent) 8%, transparent);
	}
	.sugg input {
		width: 15px;
		height: 15px;
		margin: 0;
		accent-color: var(--accent);
		cursor: pointer;
	}
	.kind {
		color: var(--text-2);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.diff {
		display: flex;
		align-items: center;
		gap: 7px;
		min-width: 0;
		color: var(--text-3);
	}
	.diff :global(svg) {
		flex: none;
	}
	.was {
		flex: 0 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.was.empty {
		font-style: italic;
		flex: none;
	}
	s.was {
		text-decoration-color: color-mix(in oklab, var(--text-3) 70%, transparent);
	}
	.now {
		flex: 0 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--text-2);
		transition: color 140ms;
	}
	.on .now {
		color: var(--text);
		font-weight: 560;
	}
	.change .now {
		color: color-mix(in oklab, hsl(40 90% 70%) 70%, var(--text-2));
	}
	.on.change .now {
		color: hsl(40 92% 72%);
	}
	/* A hidden copy of the value sizes the input to its text. */
	.grow {
		display: inline-grid;
		min-width: 6ch;
	}
	.grow::after,
	.grow input {
		grid-area: 1 / 1;
		min-width: 0;
		padding: 1px 5px;
		border: 1px solid transparent;
	}
	.grow::after {
		content: attr(data-value) ' ';
		overflow: hidden;
		white-space: pre;
		visibility: hidden;
	}
	.grow input {
		width: 100%;
		height: 100%;
		margin: 0;
		border-radius: 6px;
		background: none;
		outline: none;
		text-overflow: ellipsis;
		cursor: text;
		transition:
			border-color 140ms,
			background 140ms;
	}
	.grow input:hover {
		border-color: var(--border-strong);
	}
	.grow input:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--surface);
	}
	.grow input::placeholder {
		color: var(--text-3);
		font-weight: 400;
	}
	.reset {
		flex: none;
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 6px;
		color: var(--text-3);
		transition:
			color 140ms,
			background 140ms;
	}
	.reset:hover {
		color: var(--text);
		background: var(--surface-hover);
	}
	.per-track {
		display: grid;
		gap: 2px;
		/* Under the tag's name, past the checkbox. */
		margin: 0 0 4px 33px;
		padding-left: 10px;
		border-left: 1px solid var(--border);
	}
	.track-value {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) minmax(0, 1.6fr);
		align-items: center;
		gap: 8px;
		padding: 1px 8px 1px 0;
		font-size: 12.5px;
	}
	.track-value .track-title {
		color: var(--text-2);
		font-weight: 450;
	}
	.mono {
		font-family: ui-monospace, monospace;
		font-size: 12px;
	}
	.roman {
		display: grid;
		flex: none;
		color: var(--accent);
	}
	.script {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.segmented {
		display: inline-flex;
		flex: none;
		padding: 2px;
		border-radius: 9px;
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.segmented button {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		height: 26px;
		padding: 0 10px;
		border-radius: 7px;
		font-size: 12.5px;
		color: var(--text-3);
		transition:
			background 160ms,
			color 160ms;
	}
	.segmented button:hover:not(.on) {
		color: var(--text);
	}
	.segmented .on {
		background: color-mix(in oklab, var(--accent) 22%, var(--surface));
		color: var(--text);
	}
	.script-note {
		flex: 1 1 240px;
		font-size: 12.5px;
		color: var(--text-3);
	}
	.others {
		flex: none;
		display: inline-flex;
		align-items: center;
		gap: 3px;
		padding: 1px 6px;
		border-radius: 999px;
		font-size: 11px;
		color: var(--text-3);
		background: var(--surface-hover);
		transition:
			color 140ms,
			background 140ms;
	}
	.others:hover,
	.others[aria-expanded='true'] {
		color: var(--text);
		background: color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.thumb {
		flex: none;
		display: grid;
		width: 48px;
		height: 48px;
		border-radius: 7px;
		overflow: hidden;
		background: var(--surface);
		box-shadow: 0 0 0 1px var(--border);
		cursor: zoom-in;
		transition:
			box-shadow 140ms,
			transform 140ms var(--ease-out);
	}
	.thumb:hover {
		box-shadow: 0 0 0 1px var(--border-strong);
		transform: scale(1.04);
	}
	.thumb img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.fetching {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 12.5px;
	}
	.art-error {
		font-size: 12.5px;
		color: var(--danger);
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
	.chev {
		display: grid;
		transition: transform 220ms var(--ease-out);
	}
	.chev.open {
		transform: rotate(180deg);
	}
	.scope {
		font-size: 11.5px;
		color: var(--text-3);
		white-space: nowrap;
	}
	.agree {
		display: flex;
		align-items: center;
		gap: 7px;
		margin: 0;
		font-size: 13px;
		color: var(--success);
	}

	@media (max-width: 640px) {
		.sugg {
			grid-template-columns: auto minmax(0, 1fr);
		}
		.diff {
			grid-column: 2;
		}
		.scope {
			display: none;
		}
		.per-track {
			margin-left: 8px;
		}
		.track-value {
			grid-template-columns: auto minmax(0, 1fr);
		}
		.track-value .diff {
			grid-column: 2;
		}
	}
</style>
