<script lang="ts">
	import Play from '@lucide/svelte/icons/play';
	import Pause from '@lucide/svelte/icons/pause';
	import Pencil from '@lucide/svelte/icons/pencil';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import ListMusic from '@lucide/svelte/icons/list-music';
	import X from '@lucide/svelte/icons/x';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import { fade } from 'svelte/transition';
	import {
		coverUrl,
		type Album,
		type AlbumDetail,
		type ImportEdits,
		type TagWriteResult
	} from '#lib/api.ts';
	import { accent, shade } from '#lib/color.ts';
	import { duration, longDuration, quality } from '#lib/format.ts';
	import { player } from '#lib/player.svelte.ts';
	import TagEditor from './TagEditor.svelte';

	interface Props {
		album: Album;
		detail: AlbumDetail | null;
		allowWrites: boolean;
		onclose: () => void;
		onsaved: (result: TagWriteResult) => void;
		/** Present for Inbox albums: bring this album into the Library, with any tag edits. */
		onimport?: (edits?: ImportEdits) => void;
	}
	let { album, detail, allowWrites, onclose, onsaved, onimport }: Props = $props();

	let mode = $state<'tracks' | 'edit'>('tracks');
	let closeButton: HTMLButtonElement;
	let heroLoaded = $state(false);

	let discs = $derived.by(() => {
		const groups = new Map<number, AlbumDetail['tracks']>();
		for (const t of detail?.tracks ?? []) {
			const d = t.disc_no ?? 1;
			if (!groups.has(d)) groups.set(d, []);
			groups.get(d)!.push(t);
		}
		return [...groups.entries()];
	});
	let isThisAlbum = $derived(player.album?.id === album.id);
	let first = $derived(detail?.tracks[0]);

	function play(index = 0) {
		if (!detail) return;
		if (isThisAlbum && index === player.index) player.toggle();
		else player.playAlbum(album, detail.tracks, index);
	}

	$effect(() => closeButton?.focus({ preventScroll: true }));
</script>

<div
	class="sheet-wrap"
	style:--tint={shade(album.color, 14)}
	style:--accent={accent(album.color)}
	onclick={(e) => e.target === e.currentTarget && onclose()}
	role="presentation"
>
	<div class="sheet" role="dialog" aria-modal="true" aria-labelledby="album-title">
		<div class="backdrop" aria-hidden="true">
			{#if album.has_cover}<img src={coverUrl(album.id, 256)} alt="" />{/if}
		</div>
		<button class="icon-btn close" bind:this={closeButton} onclick={onclose} aria-label="Close">
			<X size={20} />
		</button>

		<header class="hero">
			<div
				class="hero-cover"
				style:view-transition-name="album-cover"
				style:background-image={album.has_cover ? `url(${coverUrl(album.id, 256)})` : 'none'}
			>
				{#if album.has_cover}
					<img
						src={coverUrl(album.id, 1024)}
						alt="Cover of {album.title}"
						class:loaded={heroLoaded}
						onload={() => (heroLoaded = true)}
					/>
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
					<div class="mode" role="tablist" aria-label="View">
						<span class="thumb" class:right={mode === 'edit'}></span>
						<button
							role="tab"
							aria-selected={mode === 'tracks'}
							class:on={mode === 'tracks'}
							onclick={() => (mode = 'tracks')}
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
					</div>
				</div>
			</div>
		</header>

		<div class="body">
			{#if !detail}
				<div class="skeleton" aria-busy="true">
					{#each { length: Math.min(album.track_count, 10) } as _, i (i)}
						<span style:--i={i}></span>
					{/each}
				</div>
			{:else if mode === 'tracks'}
				<div in:fade={{ duration: 200 }}>
					{#each discs as [disc, tracks] (disc)}
						{#if discs.length > 1}<h3 class="disc">Disc {disc}</h3>{/if}
						<ol class="tracks">
							{#each tracks as t (t.id)}
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
							{/each}
						</ol>
					{/each}
					<p class="path" title={detail.dir}>{`\u200e${detail.dir}\u200e`}</p>
				</div>
			{:else}
				<div in:fade={{ duration: 200 }}>
					{#key detail}
						<TagEditor
							{detail}
							{allowWrites}
							{onsaved}
							{onimport}
							oncancel={() => (mode = 'tracks')}
						/>
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
		background: color-mix(in oklab, var(--bg) 55%, transparent);
		backdrop-filter: blur(18px) saturate(1.2);
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
	.backdrop img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		filter: blur(60px) saturate(1.4);
		opacity: 0.55;
		transform: scale(1.3);
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
