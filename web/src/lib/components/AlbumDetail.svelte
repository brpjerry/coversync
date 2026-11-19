<script lang="ts">
	import Play from '@lucide/svelte/icons/play';
	import Pause from '@lucide/svelte/icons/pause';
	import Pencil from '@lucide/svelte/icons/pencil';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import ListMusic from '@lucide/svelte/icons/list-music';
	import X from '@lucide/svelte/icons/x';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import { tick, untrack } from 'svelte';
	import { expoOut } from 'svelte/easing';
	import { fade, slide } from 'svelte/transition';
	import {
		coverUrl,
		type Album,
		type AlbumDetail,
		type ImportEdits,
		type TagWriteResult
	} from '#lib/api.ts';
	import { accent, shade } from '#lib/color.ts';
	import { duration, longDuration, quality } from '#lib/format.ts';
	import { describeIncomplete, unplacedText, withGaps } from '#lib/incomplete.ts';
	import { player } from '#lib/player.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import { lostTracks } from '#lib/tag-form.ts';
	import TagEditor, { type Draft, type TagDraft, type TagView } from './TagEditor.svelte';

	interface Props {
		album: Album;
		detail: AlbumDetail | null;
		onclose: () => void;
		/** A save of the album's tags, answered; `id` is the album's id as it was. */
		onsaved: (result: TagWriteResult, id: string) => void;
		/**
		 * Present for Inbox albums: bring this album into the Library, with
		 * any tag edits. Their tags are edited only on the way (see `edit`).
		 */
		onimport?: (edits?: ImportEdits) => void;
		/** The album changed elsewhere (while its tags were being edited here, say); the sheet shows it as it was. */
		changed?: boolean;
		/**
		 * Show the album as it is now: dropping the edits (Reload), or with
		 * the kept ones (`keepDraft`: the editor's Try again, after the album
		 * changed while its tags were read; `from` is the album it showed).
		 */
		onreload?: (keepDraft?: boolean, from?: AlbumDetail) => void;
		/** Hears what edits the tag editor has. */
		ondraft?: (draft: Draft) => void;
		/**
		 * Edits from a save refused because the album had changed elsewhere, on top
		 * of `detail`, the album as it is now (see TagEditor). The notice says so.
		 */
		kept?: TagDraft | null;
		/** A tag save was refused (see TagEditor). */
		onrefused?: (current: AlbumDetail | null, draft: () => TagDraft | null, id: string) => void;
		/** The tag editor's edits were dropped (Revert, Done, or the Tracks view): `kept` with them. */
		ondiscard?: () => void;
	}
	let {
		album,
		detail,
		onclose,
		onsaved,
		onimport,
		changed = false,
		onreload,
		ondraft,
		kept = null,
		onrefused,
		ondiscard
	}: Props = $props();

	/**
	 * What the tag editor has open, kept while it's shown: a new one (made
	 * when the album changes elsewhere, or a save is refused; one that saved
	 * reads the album in place) opens the same way.
	 */
	let editorView = $state<TagView | null>(null);
	/**
	 * While a new editor reads the tags it's short: its box keeps the height
	 * the editor had, so the page doesn't jump (and the scroll stays put).
	 * The height is held all along, following the editor's own except while
	 * one loads, so it's already there the moment an editor is replaced.
	 */
	let editorLoading = $state(false);
	let editorHeight = $state(0);
	let heldHeight = $state(0);
	$effect(() => {
		if (!editorLoading && editorHeight > 0) heldHeight = editorHeight;
	});

	/**
	 * The album the tag editor shows. A new version makes it anew (its kept
	 * edits put on the album as it is, say); the one its own save answers is
	 * handed to it to read in place, so nothing flashes.
	 */
	let editorDetail = $state(untrack(() => detail));
	let editorKey = $state(0);
	let ownSave = false;
	$effect(() => {
		const next = detail;
		untrack(() => {
			if (next === editorDetail) return;
			// One album's version for another's: a new editor. Across no album
			// at all, the sheet makes the editor anew itself.
			if (next && editorDetail && !ownSave) editorKey++;
			// Another album comes (from the player bar, say): its editor opens
			// afresh, not as this one's was.
			if (!next) editorView = null;
			ownSave = false;
			editorDetail = next;
		});
	});
	async function saved(result: TagWriteResult, id: string) {
		ownSave = true;
		onsaved(result, id);
		// Answered with the album as saved by now, or not at all.
		await tick();
		ownSave = false;
	}

	/** Leave the tag editor: its edits go, kept ones included. */
	function showTracks() {
		if (mode === 'edit') ondiscard?.();
		editorView = null;
		mode = 'tracks';
	}

	let mode = $state<'tracks' | 'edit'>('tracks');
	/** Open the tag editor: where an Inbox album's import can start. */
	export function edit() {
		mode = 'edit';
	}
	// Another album's editor doesn't stay open for an Inbox album.
	$effect(() => {
		void album.id;
		untrack(() => {
			if (onimport) mode = 'tracks';
		});
	});
	/** The tag editor's edits, for the notice. */
	let editing = $state<Draft>('none');
	/** Kept edits for tracks no longer in the album, which couldn't be put back. */
	let dropped = $derived(kept && detail ? lostTracks(kept, detail.tracks) : 0);
	let closeButton: HTMLButtonElement;
	let heroLoaded = $state(false);

	let discs = $derived.by(() => {
		const groups = new Map<number, AlbumDetail['tracks']>();
		for (const t of detail?.tracks ?? []) {
			// Disc 0 is no disc, as the server sorts it.
			const d = t.disc_no || 1;
			if (!groups.has(d)) groups.set(d, []);
			groups.get(d)!.push(t);
		}
		return [...groups.entries()];
	});
	/** What the album's numbering says it lacks, as it is now. */
	let incomplete = $derived(detail ? detail.incomplete : album.incomplete);
	/** The discs' tracks with rows for the numbers missing between them, and missing discs. */
	let rows = $derived(withGaps(discs, incomplete));
	let isThisAlbum = $derived(player.album?.id === album.id);
	let first = $derived(detail?.tracks[0]);

	function play(index = 0) {
		if (!detail) return;
		if (isThisAlbum && index === player.index) player.toggle();
		else player.playAlbum(album, detail.tracks, index);
	}

	$effect(() => closeButton?.focus({ preventScroll: true }));

	/** Whether a view transition is moving the cover between the grid and the sheet. */
	function morphing() {
		try {
			return document.documentElement.matches(':active-view-transition');
		} catch {
			return false; // Without the selector the whole window crossfades instead (see app.css).
		}
	}
</script>

<!-- Fades in and out only while its cover moves (see app.css). Fading out to
     the grid, it hides its own cover (.closing, set by the page's close()),
     which the moving snapshot stands for; a fade that turns back (Forward, say)
     shows it again. -->
<div
	class="sheet-wrap"
	transition:fade|global={{ duration: morphing() ? 280 : 0, easing: expoOut }}
	onintrostart={(e) => e.currentTarget.classList.remove('closing')}
	style:--tint={shade(album.color, 14)}
	style:--accent={accent(album.color)}
	onclick={(e) => e.target === e.currentTarget && onclose()}
	role="presentation"
>
	<div class="sheet" role="dialog" aria-modal="true" aria-labelledby="album-title">
		<!-- Covers asked for while signed out failed: signing back in asks again. -->
		<div class="backdrop" aria-hidden="true">
			{#key session.generation}
				{#if album.has_cover}<img src={coverUrl(album, 256)} alt="" />{/if}
			{/key}
		</div>
		<button class="icon-btn close" bind:this={closeButton} onclick={onclose} aria-label="Close">
			<X size={20} />
		</button>

		<header class="hero">
			<div
				class="hero-cover"
				style:view-transition-name="album-cover"
				style:background-image={album.has_cover ? `url(${coverUrl(album, 256)})` : 'none'}
			>
				{#if album.has_cover}
					{#key session.generation}
						<img
							src={coverUrl(album, 1024)}
							alt="Cover of {album.title}"
							class:loaded={heroLoaded}
							onload={() => (heroLoaded = true)}
						/>
					{/key}
				{:else}
					<span class="placeholder"><Disc3 size={72} strokeWidth={1} /></span>
				{/if}
			</div>
			<div class="hero-text">
				<span class="kicker">{first ? quality(first) : album.format}</span>
				<h2 id="album-title">{album.title}</h2>
				<p class="artist">{album.artist}</p>
				<p class="facts tabular">
					{#if album.year}<span>{album.year}</span>{/if}
					{#if album.genre}<span>{album.genre}</span>{/if}
					<span>{album.track_count} {album.track_count === 1 ? 'track' : 'tracks'}</span>
					<span>{longDuration(album.duration_ms)}</span>
					{#if incomplete}
						<span class="incomplete" title={describeIncomplete(incomplete)}>
							<CircleAlert size={13} strokeWidth={2.2} /> Incomplete
						</span>
					{/if}
				</p>
				<div class="actions">
					<button
						class="btn primary"
						onclick={() => play(isThisAlbum ? player.index : 0)}
						disabled={!detail}
					>
						{#if isThisAlbum && player.playing}
							<Pause size={16} fill="currentColor" strokeWidth={0} /> Pause
						{:else}
							<Play size={16} fill="currentColor" strokeWidth={0} />
							{isThisAlbum ? 'Resume' : 'Play'}
						{/if}
					</button>
					<!-- The tag editor has its own Import, which carries the edits. -->
					{#if onimport && mode !== 'edit'}
						<button class="btn import" onclick={() => onimport()} disabled={!detail}>
							<FolderInput size={16} /> Import
						</button>
					{/if}
					{#if !onimport}<div class="mode" role="tablist" aria-label="View">
							<span class="thumb" class:right={mode === 'edit'}></span>
							<button
								role="tab"
								aria-selected={mode === 'tracks'}
								class:on={mode === 'tracks'}
								onclick={showTracks}
							>
								<ListMusic size={15} /> Tracks
							</button>
							<button
								role="tab"
								aria-selected={mode === 'edit'}
								class:on={mode === 'edit'}
								onclick={() => (mode = 'edit')}
								disabled={!detail}
							>
								<Pencil size={15} /> Edit tags
							</button>
						</div>{/if}
				</div>
			</div>
		</header>

		<div class="body">
			{#if changed}
				<p class="elsewhere" role="status" transition:slide={{ duration: 200 }}>
					<CircleAlert size={15} strokeWidth={2.2} />
					<span
						>This album was changed elsewhere.{kept && editing !== 'none'
							? ' It shows those changes, marked, with your edits on top: save to apply them.'
							: ''}{kept && dropped
							? ` Edits to ${dropped} ${dropped === 1 ? 'track' : 'tracks'} no longer in it were dropped.`
							: ''}</span
					>
					<button class="link" title="Your edits here are dropped" onclick={() => onreload?.()}>
						Reload
					</button>
				</p>
			{/if}
			{#if !detail}
				<div class="skeleton" aria-busy="true">
					{#each { length: Math.min(album.track_count, 10) } as _, i (i)}
						<span style:--i={i}></span>
					{/each}
				</div>
			{:else if mode === 'tracks'}
				<div in:fade={{ duration: 200 }}>
					{#each rows as { key, label, rows: items } (key)}
						{#if rows.length > 1}<h3 class="disc">{label}</h3>{/if}
						<ol class="tracks">
							{#if !items}
								<li class="missing" style:--i={0}>
									<span class="row">
										<span class="no tabular">–</span>
										<span class="title"><span class="name">Not in this album</span></span>
									</span>
								</li>
							{:else}
								{#each items as item, j ('track' in item ? item.track.id : `gap ${item.from}`)}
									{#if 'track' in item}
										{@const t = item.track}
										{@const index = detail.tracks.indexOf(t)}
										{@const current = player.isCurrent(t)}
										<li style:--i={Math.min(index, 24)}>
											<button class="row" class:current onclick={() => play(index)}>
												<span class="no tabular">
													{#if current && player.playing}
														<span class="eq" aria-label="Playing"><i></i><i></i><i></i></span>
													{:else}
														<span class="num">{t.track_no ?? index + 1}</span>
														<span class="hover-play"
															><Play size={14} fill="currentColor" strokeWidth={0} /></span
														>
													{/if}
												</span>
												<span class="title">
													<span class="name">{t.title}</span>
													{#if t.artist !== album.artist}<span class="sub">{t.artist}</span>{/if}
												</span>
												<span class="dur tabular">{duration(t.duration_ms)}</span>
											</button>
										</li>
									{:else}
										<!-- A run of numbers the album's numbering says it lacks. -->
										<li class="missing" style:--i={Math.min(j, 24)}>
											<span class="row">
												<span class="no tabular"
													>{item.from === item.to ? item.from : `${item.from}–${item.to}`}</span
												>
												<span class="title">
													<span class="name"
														>{item.from === item.to
															? 'Missing'
															: `${item.to - item.from + 1} tracks missing`}</span
													>
												</span>
											</span>
										</li>
									{/if}
								{/each}
							{/if}
						</ol>
					{/each}
					{#if incomplete?.unplaced}
						<!-- More missing than the gaps hold, by an album-wide total: on no disc in particular. -->
						<ol class="tracks">
							<li class="missing" style:--i={0}>
								<span class="row">
									<span class="no tabular">–</span>
									<span class="title">
										<span class="name"
											>Missing {unplacedText(
												incomplete.unplaced,
												incomplete.tracks.length > 0 || incomplete.discs.length > 0
											)}</span
										>
									</span>
								</span>
							</li>
						</ol>
					{/if}
					<p class="path" title={detail.dir}>{`\u200e${detail.dir}\u200e`}</p>
				</div>
			{:else}
				<div
					in:fade={{ duration: 200 }}
					style:min-height={heldHeight && editorView ? `${heldHeight}px` : null}
				>
					{#key editorKey}
						<div bind:offsetHeight={editorHeight}>
							<TagEditor
								detail={editorDetail ?? detail}
								onsaved={saved}
								restore={editorView}
								onview={(v) => (editorView = v)}
								onloading={(l) => (editorLoading = l)}
								{onimport}
								ondraft={(d) => {
									editing = d;
									ondraft?.(d);
								}}
								{kept}
								{onrefused}
								onrevert={() => ondiscard?.()}
								onretry={onreload && (() => onreload?.(true, detail ?? undefined))}
								oncancel={showTracks}
							/>
						</div>
					{/key}
				</div>
			{/if}
		</div>
	</div>
</div>

<style>
	.sheet-wrap {
		position: fixed;
		inset: 0;
		z-index: 30;
		overflow-y: auto;
		overscroll-behavior: contain;
		/* Room for the player while it's showing (--dock). */
		padding: 48px 24px calc(var(--dock, 0px) + 40px);
		/* Over the page, which blurs itself while the sheet is open (.page.behind). */
		background: color-mix(in oklab, var(--bg) 55%, transparent);
	}
	.sheet {
		position: relative;
		max-width: 1080px;
		margin: 0 auto;
		border-radius: 28px;
		background: var(--bg-raised);
		border: 1px solid var(--border);
		box-shadow: 0 40px 120px -30px rgb(0 0 0 / 0.7);
		overflow: clip;
		isolation: isolate;
	}
	.backdrop {
		position: absolute;
		inset: 0 0 auto;
		height: 420px;
		z-index: -1;
		background: var(--tint);
		overflow: hidden;
	}
	/* Blurred at an eighth of its size, then scaled up: the picture of a full-size image
	   scaled by 1.3 about its centre with a 60px blur, but a blurred cover has no detail to
	   lose, and the blur is about a fiftieth of the work. Full size, WebKit (the Linux
	   desktop app) stalled 150-700 ms drawing it as the sheet opened. */
	.backdrop img {
		width: 12.5%;
		height: 12.5%;
		object-fit: cover;
		filter: blur(7.5px) saturate(1.4);
		opacity: 0.55;
		transform-origin: 0 0;
		transform: translate(-120%, -120%) scale(10.4);
	}
	.backdrop::after {
		content: '';
		position: absolute;
		inset: 0;
		background: linear-gradient(to bottom, transparent 20%, var(--bg-raised));
	}
	.close {
		position: absolute;
		top: 16px;
		right: 16px;
		z-index: 2;
		background: rgb(0 0 0 / 0.25);
		color: #fff;
		backdrop-filter: blur(10px);
	}

	.hero {
		display: grid;
		grid-template-columns: 280px 1fr;
		gap: 36px;
		align-items: end;
		padding: 48px 48px 28px;
	}
	/* The cover's name goes too: only one element may have it as the transition ends. */
	.sheet-wrap:global(.closing) .hero-cover {
		view-transition-name: none !important;
		visibility: hidden;
	}
	.hero-cover {
		position: relative;
		aspect-ratio: 1;
		border-radius: var(--radius);
		overflow: hidden;
		background: var(--tint) center / cover;
		box-shadow: 0 30px 60px -20px rgb(0 0 0 / 0.75);
	}
	.hero-cover img {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
		object-fit: cover;
		opacity: 0;
		transition: opacity 400ms;
	}
	.hero-cover img.loaded {
		opacity: 1;
	}
	.placeholder {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		color: color-mix(in oklab, var(--accent) 45%, transparent);
	}
	.hero-text {
		min-width: 0;
		animation: slide-in 600ms var(--ease-out) 80ms both;
	}
	@keyframes slide-in {
		from {
			opacity: 0;
			transform: translateY(12px);
		}
	}
	.kicker {
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--accent);
	}
	h2 {
		margin: 6px 0 4px;
		font-size: clamp(28px, 4vw, 46px);
		line-height: 1.05;
		letter-spacing: -0.03em;
		font-weight: 760;
		text-wrap: balance;
	}
	.artist {
		margin: 0;
		font-size: 18px;
		font-weight: 550;
		color: var(--text);
		opacity: 0.85;
	}
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: 0 14px;
		margin: 10px 0 0;
		color: var(--text-2);
		font-size: 13.5px;
	}
	.facts span + span::before {
		content: '·';
		margin-right: 14px;
		color: var(--text-3);
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		margin-top: 22px;
		align-items: center;
	}
	.actions .btn.primary {
		height: 44px;
		padding: 0 22px;
		font-size: 15px;
	}
	.actions .btn.import {
		height: 44px;
		padding: 0 18px;
		font-size: 15px;
	}
	.mode {
		position: relative;
		display: grid;
		grid-template-columns: 1fr 1fr;
		padding: 3px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.mode button {
		position: relative;
		z-index: 1;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		height: 36px;
		padding: 0 16px;
		border-radius: 999px;
		font-size: 13.5px;
		font-weight: 560;
		color: var(--text-2);
		transition: color 200ms;
	}
	.mode button.on {
		color: var(--text);
	}
	.mode button:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.mode .thumb {
		position: absolute;
		top: 3px;
		bottom: 3px;
		left: 3px;
		width: calc(50% - 3px);
		border-radius: 999px;
		background: var(--surface-hover);
		box-shadow: inset 0 0 0 1px var(--border-strong);
		transition: transform 380ms var(--ease-out);
	}
	.mode .thumb.right {
		transform: translateX(100%);
	}

	.body {
		padding: 8px 32px 36px;
		min-height: 200px;
	}
	.elsewhere {
		display: flex;
		align-items: center;
		gap: 10px;
		margin: 8px 0 14px;
		padding: 8px 8px 8px 14px;
		border-radius: 12px;
		font-size: 13.5px;
		color: var(--text-2);
		background: var(--warn-surface);
		border: 1px solid var(--warn-border);
	}
	.elsewhere :global(svg) {
		flex: none;
		color: var(--warn);
	}
	.elsewhere .link {
		margin-left: auto;
		padding: 4px 10px;
		border-radius: 999px;
		font-size: 13px;
		font-weight: 600;
		color: var(--accent);
		white-space: nowrap;
		transition: background 140ms;
	}
	.elsewhere .link:hover {
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}
	.disc {
		margin: 18px 16px 6px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	.tracks {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.tracks li {
		animation: row-in 460ms var(--ease-out) both;
		animation-delay: calc(120ms + var(--i) * 22ms);
	}
	@keyframes row-in {
		from {
			opacity: 0;
			transform: translateY(8px);
		}
	}
	.row {
		width: 100%;
		display: grid;
		grid-template-columns: 40px 1fr auto;
		align-items: center;
		gap: 12px;
		padding: 9px 16px;
		border-radius: 12px;
		text-align: left;
		transition: background 140ms;
	}
	.row:hover {
		background: var(--surface);
	}
	.row.current {
		background: color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.row.current .name,
	.row.current .no {
		color: var(--accent);
	}
	.no {
		position: relative;
		display: grid;
		place-items: center;
		color: var(--text-3);
		font-size: 13.5px;
	}
	.hover-play {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		opacity: 0;
		color: var(--text);
	}
	.row:hover .num {
		opacity: 0;
	}
	.row:hover .hover-play {
		opacity: 1;
	}
	.title {
		display: grid;
		min-width: 0;
	}
	.name {
		font-weight: 520;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.sub {
		font-size: 13px;
		color: var(--text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.dur {
		color: var(--text-3);
		font-size: 13.5px;
	}
	/* Tracks (or a disc) the album's numbering says it lacks: a placeholder, not a row to play. */
	.missing .row:hover {
		background: none;
	}
	.missing .no {
		white-space: nowrap;
		font-size: 12.5px;
	}
	.missing .name {
		white-space: normal;
		font-weight: 450;
		font-style: italic;
		color: var(--text-3);
	}
	.facts .incomplete {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		color: var(--warn);
		cursor: help;
	}
	.eq {
		display: flex;
		align-items: flex-end;
		gap: 2px;
		height: 14px;
	}
	.eq i {
		width: 3px;
		height: 100%;
		border-radius: 2px;
		background: var(--accent);
		transform-origin: bottom;
		animation: eq 800ms ease-in-out infinite alternate;
	}
	.eq i:nth-child(2) {
		animation-delay: -260ms;
	}
	.eq i:nth-child(3) {
		animation-delay: -520ms;
	}
	@keyframes eq {
		from {
			transform: scaleY(0.2);
		}
	}
	.path {
		margin: 24px 16px 0;
		font-size: 12px;
		color: var(--text-3);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		direction: rtl;
		text-align: left;
	}

	.skeleton {
		display: grid;
		gap: 8px;
		padding: 8px 16px;
	}
	.skeleton span {
		height: 36px;
		border-radius: 10px;
		background: linear-gradient(
				90deg,
				var(--surface) 0%,
				var(--surface-hover) 50%,
				var(--surface) 100%
			)
			0 0 / 200% 100%;
		animation: shimmer 1.4s linear infinite;
		animation-delay: calc(var(--i) * 60ms);
	}
	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}

	@media (max-width: 760px) {
		.sheet-wrap {
			padding: 0 0 var(--dock, 0px);
		}
		.sheet {
			border-radius: 0;
			min-height: 100dvh;
		}
		.hero {
			grid-template-columns: 1fr;
			padding: 64px 20px 20px;
		}
		.hero-cover {
			width: min(280px, 70vw);
		}
		.body {
			padding: 8px 4px 24px;
		}
	}
</style>
