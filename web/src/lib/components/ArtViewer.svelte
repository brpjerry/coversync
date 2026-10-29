<script lang="ts" module>
	export interface ViewedArt {
		/** e.g. "Original" or "MusicBrainz". */
		label: string;
		url: string;
		width: number | null;
		height: number | null;
		/** Where it comes from, e.g. "cover.jpg". */
		note?: string;
	}
</script>

<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { fade, scale } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import X from '@lucide/svelte/icons/x';

	interface Props {
		images: ViewedArt[];
		/** The image shown first. */
		start?: number;
		onclose: () => void;
	}
	let { images, start = 0, onclose }: Props = $props();

	// Opens on `start`, then switches on its own.
	let index = $state(untrack(() => start));
	let shown = $derived(images[Math.min(index, images.length - 1)]);
	let el = $state<HTMLElement>();

	const size = (a: ViewedArt) =>
		a.width && a.height ? `${a.width} × ${a.height}` : 'size unknown';

	onMount(() => {
		// Focus comes back to the thumbnail that opened it.
		const before = document.activeElement as HTMLElement | null;
		el?.focus();
		return () => before?.focus({ preventScroll: true });
	});

	// The import dialog underneath would close on Escape and import on Ctrl+Enter.
	function onkeydown(e: KeyboardEvent) {
		e.stopPropagation();
		if (e.key === 'Escape') onclose();
		else if (e.key === 'ArrowLeft') index = Math.max(0, index - 1);
		else if (e.key === 'ArrowRight') index = Math.min(images.length - 1, index + 1);
	}
</script>

<div
	class="viewer"
	role="dialog"
	aria-modal="true"
	aria-label="{shown.label} album art"
	tabindex="-1"
	bind:this={el}
	{onkeydown}
	onclick={(e) => e.target === e.currentTarget && onclose()}
	transition:fade={{ duration: 160 }}
>
	<figure transition:scale={{ start: 0.96, duration: 220, easing: cubicOut }}>
		{#key shown.url}
			<img src={shown.url} alt="{shown.label} album art" in:fade={{ duration: 160 }} />
		{/key}
		<figcaption>
			{#if images.length > 1}
				<div class="segmented" role="radiogroup" aria-label="Album art to show">
					{#each images as image, i (image.url)}
						<button
							role="radio"
							aria-checked={i === index}
							class:on={i === index}
							onclick={() => (index = i)}
						>
							{image.label} <span class="tabular">{size(image)}</span>
						</button>
					{/each}
				</div>
			{:else}
				<b>{shown.label}</b> <span class="tabular">{size(shown)}</span>
			{/if}
			{#if shown.note}<span class="note">{shown.note}</span>{/if}
		</figcaption>
	</figure>
	<button class="icon-btn close" onclick={onclose} aria-label="Close"><X size={19} /></button>
</div>

<style>
	.viewer {
		position: fixed;
		inset: 0;
		z-index: 60;
		display: grid;
		place-items: center;
		padding: 24px;
		background: rgb(0 0 0 / 0.82);
		outline: none;
	}
	figure {
		display: grid;
		justify-items: center;
		gap: 14px;
		margin: 0;
		min-width: 0;
		pointer-events: none;
	}
	figure > * {
		pointer-events: auto;
	}
	img {
		display: block;
		/* Never larger than it is, so the two can be compared fairly. */
		max-width: min(100%, calc(100vw - 48px));
		max-height: calc(100dvh - 140px);
		border-radius: 8px;
		box-shadow: 0 30px 90px -20px rgb(0 0 0 / 0.8);
		background: var(--surface);
	}
	figcaption {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		align-items: center;
		gap: 8px 14px;
		font-size: 13px;
		color: rgb(255 255 255 / 0.85);
	}
	.note {
		color: rgb(255 255 255 / 0.6);
	}
	.segmented {
		display: inline-flex;
		padding: 2px;
		border-radius: 10px;
		background: rgb(255 255 255 / 0.08);
		border: 1px solid rgb(255 255 255 / 0.12);
	}
	.segmented button {
		display: inline-flex;
		align-items: baseline;
		gap: 6px;
		height: 28px;
		padding: 0 12px;
		border-radius: 8px;
		font-size: 13px;
		color: rgb(255 255 255 / 0.65);
		transition:
			background 160ms,
			color 160ms;
	}
	.segmented button:hover:not(.on) {
		color: #fff;
	}
	.segmented .on {
		background: rgb(255 255 255 / 0.16);
		color: #fff;
	}
	.segmented .tabular {
		font-size: 12px;
		opacity: 0.8;
	}
	.close {
		position: absolute;
		top: 16px;
		right: 16px;
		color: #fff;
	}
</style>
