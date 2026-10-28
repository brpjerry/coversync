<script lang="ts">
	import { onMount } from 'svelte';
	import type { Album } from '#lib/api.ts';
	import { GRID_MAX_WIDTH, gridLayout } from '#lib/grid.ts';
	import AlbumCard from './AlbumCard.svelte';

	interface Props {
		albums: Album[];
		/** The narrowest a cover may be, in px (Appearance's cover size). */
		size: number;
		onopen: (album: Album, cover: HTMLElement) => void;
		onplay: (album: Album) => void;
		onmenu: (album: Album, x: number, y: number) => void;
		/** A warning badge for a card, if any: short text plus a tooltip. */
		flag?: (album: Album) => { text: string; title: string } | null;
	}
	let { albums, size, onopen, onplay, onmenu, flag }: Props = $props();

	// Only the rows near the viewport are in the DOM, each card placed by
	// transform. Reordering then touches a few dozen elements instead of every
	// album, and the cards that stay on screen glide to their new spots.
	const OVERSCAN = 2;

	let grid = $state<HTMLElement>();
	let width = $state(0);
	/** The grid's distance from the top of the document. */
	let top = $state(0);
	let scrollY = $state(0);
	let viewport = $state(900);
	let meta = $state(50);

	let { colGap, rowGap, cols, cellWidth } = $derived(gridLayout(width, size));
	let rowHeight = $derived(cellWidth + meta + rowGap);
	let rows = $derived(Math.ceil(albums.length / cols));

	let first = $derived(Math.max(0, Math.floor((scrollY - top) / rowHeight) - OVERSCAN));
	let last = $derived(
		Math.min(rows - 1, Math.ceil((scrollY + viewport - top) / rowHeight) + OVERSCAN)
	);
	let shown = $derived(
		albums
			.slice(first * cols, (last + 1) * cols)
			.map((album, i) => ({ album, index: first * cols + i }))
	);

	/** Staggered entrance on first render; afterwards new cards just fade in. */
	let intro = $state(true);
	/** No gliding while the width changes (window resize, sidebar collapse). */
	let settling = $state(true);

	onMount(() => {
		const el = grid!;
		let frame = 0;
		let settle: ReturnType<typeof setTimeout>;
		const measure = () => {
			width = el.clientWidth;
			top = el.getBoundingClientRect().top + window.scrollY;
			viewport = innerHeight;
			const m = el.querySelector<HTMLElement>('.meta');
			if (m) meta = m.offsetHeight;
		};
		const onscroll = () => {
			frame ||= requestAnimationFrame(() => {
				frame = 0;
				scrollY = window.scrollY;
				top = el.getBoundingClientRect().top + scrollY;
			});
		};
		const ro = new ResizeObserver(() => {
			settling = true;
			measure();
			clearTimeout(settle);
			settle = setTimeout(() => (settling = false), 250);
		});
		scrollY = window.scrollY;
		measure();
		ro.observe(el);
		addEventListener('scroll', onscroll, { passive: true });
		addEventListener('resize', measure);
		const introTimer = setTimeout(() => (intro = false), 1400);
		requestAnimationFrame(measure);
		return () => {
			ro.disconnect();
			removeEventListener('scroll', onscroll);
			removeEventListener('resize', measure);
			cancelAnimationFrame(frame);
			clearTimeout(settle);
			clearTimeout(introTimer);
		};
	});
</script>

<div
	class="vgrid"
	class:intro
	class:settling
	bind:this={grid}
	style:max-width="{GRID_MAX_WIDTH}px"
	style:height="{rows ? rows * rowHeight - rowGap : 0}px"
	role="list"
	aria-label="{albums.length} albums"
>
	{#each shown as { album, index } (album.id)}
		{@const f = flag?.(album) ?? null}
		<div
			class="cell"
			role="listitem"
			style:width="{cellWidth}px"
			style:transform="translate({(index % cols) * (cellWidth + colGap)}px, {Math.floor(
				index / cols
			) * rowHeight}px)"
			style:--i={Math.min(index, 36)}
		>
			<AlbumCard
				{album}
				width={cellWidth}
				{onopen}
				{onplay}
				{onmenu}
				flag={f?.text}
				flagTitle={f?.title}
			/>
		</div>
	{/each}
</div>

<style>
	.vgrid {
		position: relative;
		margin: 0 auto;
	}
	.cell {
		position: absolute;
		top: 0;
		left: 0;
		transition: transform 480ms var(--ease-out);
		animation: appear 360ms var(--ease-out) both;
	}
	.settling .cell {
		transition: none;
	}
	.intro .cell {
		animation: rise 560ms var(--ease-out) both;
		animation-delay: calc(var(--i) * 16ms);
	}
	@keyframes appear {
		from {
			opacity: 0;
		}
	}
	@keyframes rise {
		from {
			opacity: 0;
			translate: 0 14px;
			scale: 0.98;
		}
	}
</style>
