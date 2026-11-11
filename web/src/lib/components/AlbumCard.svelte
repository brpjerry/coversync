<script lang="ts">
	import Play from '@lucide/svelte/icons/play';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import { scale } from 'svelte/transition';
	import { coverUrl, type Album } from '#lib/api.ts';
	import { accent, shade } from '#lib/color.ts';
	import { player } from '#lib/player.svelte.ts';
	import { session } from '#lib/session.svelte.ts';

	interface Props {
		album: Album;
		/** The card's width in px, so the browser fetches a sharp enough cover. */
		width: number;
		onopen: (album: Album, cover: HTMLElement) => void;
		onplay: (album: Album) => void;
		/** Open the album's context menu at a viewport position. */
		onmenu: (album: Album, x: number, y: number) => void;
		/** Shown as a warning badge, e.g. "Genre" or "3 missing", with `flagTitle` as tooltip. */
		flag?: string | null;
		flagTitle?: string;
	}
	let { album, width, onopen, onplay, onmenu, flag = null, flagTitle }: Props = $props();

	function menuKey(e: KeyboardEvent) {
		if (e.key === 'ContextMenu' || (e.key === 'F10' && e.shiftKey)) {
			e.preventDefault();
			const r = cover.getBoundingClientRect();
			onmenu(album, r.left + r.width / 2, r.top + r.height / 2);
		}
	}

	let loaded = $state(false);
	let cached = $state(false);
	let failed = $state(false);
	// A cover asked for while signed out failed; try again once signed back in.
	$effect(() => {
		if (session.generation) failed = false;
	});
	let cover: HTMLElement;
	let playing = $derived(player.album?.id === album.id && player.playing);
</script>

<article
	class="card"
	data-album={album.id}
	style:--tint={shade(album.color, 16)}
	style:--glow={accent(album.color)}
	oncontextmenu={(e) => {
		e.preventDefault();
		onmenu(album, e.clientX, e.clientY);
	}}
>
	<div class="art">
		<button
			class="cover"
			class:loaded
			class:cached
			bind:this={cover}
			onclick={() => onopen(album, cover)}
			onkeydown={menuKey}
			aria-label="Open {album.title} by {album.artist}"
			aria-haspopup="menu"
		>
			{#if album.has_cover && !failed}
				<img
					src={coverUrl(album, 256)}
					srcset="{coverUrl(album, 256)} 256w, {coverUrl(album, 512)} 512w, {coverUrl(
						album,
						1024
					)} 1024w"
					sizes="{Math.ceil(width)}px"
					alt=""
					loading="lazy"
					decoding="async"
					onload={(e) => {
						const show = () => (loaded = true);
						// Shown only once decoded. Chromium otherwise draws the card without the
						// cover first and adds it when the decoding is done, and that second drawing
						// could go missing when a fast scroll stopped abruptly (at the top of the
						// list, or under a new touch): a cover shown without a fade stayed blank
						// until its card was hovered.
						(e.currentTarget as HTMLImageElement).decode().then(show, show);
					}}
					onerror={() => (failed = true)}
					{@attach (img: HTMLImageElement) => {
						// Cards are rebuilt as the grid scrolls; covers already in the
						// browser's cache show at once instead of fading in again.
						if (img.complete && img.naturalWidth) cached = loaded = true;
					}}
				/>
			{:else}
				<span class="placeholder" aria-hidden="true"><Disc3 size={40} strokeWidth={1.25} /></span>
			{/if}
			{#if flag}
				<span class="flag" title={flagTitle} transition:scale={{ start: 0.6, duration: 220 }}>
					<CircleAlert size={13} strokeWidth={2.2} />
					{flag}
				</span>
			{/if}
			{#if playing}
				<span class="now" aria-hidden="true"><i></i><i></i><i></i></span>
			{/if}
		</button>
		<button
			class="play"
			class:visible={playing}
			onclick={() => onplay(album)}
			aria-label="Play {album.title}"
		>
			<Play size={18} fill="currentColor" strokeWidth={0} />
		</button>
	</div>
	<div class="meta">
		<button class="title" onclick={() => onopen(album, cover)} tabindex="-1">{album.title}</button>
		<div class="artist">
			{album.artist}{#if album.year}<span class="year">{` · ${album.year}`}</span>{/if}
		</div>
	</div>
</article>

<style>
	.card {
		position: relative;
	}

	.art {
		position: relative;
		transition: transform 380ms var(--ease-out);
	}
	.card:hover .art,
	.card:focus-within .art {
		transform: translateY(-4px);
	}
	.cover {
		position: relative;
		display: block;
		width: 100%;
		aspect-ratio: 1;
		border-radius: var(--radius);
		overflow: hidden;
		background: var(--tint);
		box-shadow: 0 1px 2px rgb(0 0 0 / 0.3);
		transition: box-shadow 380ms var(--ease-out);
	}
	.cover::after {
		content: '';
		position: absolute;
		inset: 0;
		border-radius: inherit;
		box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.07);
		pointer-events: none;
	}
	.card:hover .cover,
	.card:focus-within .cover {
		box-shadow:
			0 18px 40px -14px color-mix(in oklab, var(--glow) 55%, transparent),
			0 6px 16px -6px rgb(0 0 0 / 0.5);
	}
	img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
		/* The tint .cover already shows, under an opaque cover: no visible change.
		   In WebKitGTK (the Linux desktop app then), a background keeps a cover that's
		   fading in from becoming a layer holding just the image, which WebKit
		   decodes on the page's main thread and uploads on the thread that
		   composites each frame. With it, covers decode on WebKit's decoding
		   thread and are painted with the rest of the page. A border radius
		   would do the same there, but antialiases the corners twice. */
		background: var(--tint);
		/* Covers fade in by opacity alone, so that they can while the grid scrolls
		   too. They used to sharpen from a blur and settle from a slight zoom as
		   well, and the zoom cost frames when a fast scroll brought in rows of them:
		   in the desktop app, four fast two-finger scrolls had 27-32 late frames in
		   about 800-1040 with the zoom (blurred or not), 4-9 with the blur alone,
		   some of them several refreshes long, and 3-6 with opacity alone, as many
		   as with no fade at all. */
		opacity: 0;
		transition: opacity 520ms var(--ease-out);
	}
	.loaded img {
		opacity: 1;
	}
	.cached img {
		transition: none;
	}
	.placeholder {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		color: color-mix(in oklab, var(--glow) 50%, transparent);
	}

	.play {
		position: absolute;
		right: 10px;
		bottom: 10px;
		width: 44px;
		height: 44px;
		display: grid;
		place-items: center;
		padding-left: 2px;
		border-radius: 999px;
		/* Dark ink on the dark theme's pale glow, white on the light theme's deep glow. */
		color: var(--accent-ink);
		background: var(--glow);
		box-shadow: 0 8px 20px -6px rgb(0 0 0 / 0.6);
		opacity: 0;
		transform: translateY(8px) scale(0.6);
		transition:
			opacity 200ms,
			transform 360ms var(--ease-spring);
	}
	.card:hover .play,
	.card:focus-within .play,
	.play.visible {
		opacity: 1;
		transform: none;
	}
	.card .play:hover {
		transform: scale(1.08);
	}

	.flag {
		position: absolute;
		top: 8px;
		left: 8px;
		max-width: calc(100% - 16px);
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 3px 8px 3px 6px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: hsl(40 95% 72%);
		background: rgb(20 16 6 / 0.72);
		backdrop-filter: blur(8px);
		box-shadow: inset 0 0 0 1px hsl(40 90% 60% / 0.35);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.now {
		position: absolute;
		left: 10px;
		bottom: 10px;
		display: flex;
		align-items: flex-end;
		gap: 3px;
		height: 18px;
		padding: 5px 6px;
		box-sizing: content-box;
		border-radius: 8px;
		background: rgb(0 0 0 / 0.55);
		backdrop-filter: blur(8px);
	}
	.now i {
		width: 3px;
		height: 100%;
		border-radius: 2px;
		background: var(--glow);
		transform-origin: bottom;
		animation: eq 900ms ease-in-out infinite alternate;
	}
	.now i:nth-child(2) {
		animation-delay: -300ms;
	}
	.now i:nth-child(3) {
		animation-delay: -600ms;
	}
	@keyframes eq {
		from {
			transform: scaleY(0.25);
		}
	}

	.meta {
		padding: 10px 2px 0;
		min-width: 0;
	}
	.title {
		display: block;
		max-width: 100%;
		font-weight: 600;
		font-size: 14px;
		text-align: left;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.artist {
		color: var(--text-2);
		font-size: 13px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.year {
		color: var(--text-3);
	}
</style>
