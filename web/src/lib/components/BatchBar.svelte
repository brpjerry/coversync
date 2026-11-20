<script lang="ts">
	import { fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import type { ImportBatch } from '#lib/batch.svelte.ts';
	import BatchStrip from './BatchStrip.svelte';

	// A batch with Smart suggestions off goes through each album's tag editor:
	// this bar over the album sheet is its strip, Skip and Stop. The editor's
	// own Import is the batch's Import & next.

	interface Props {
		batch: ImportBatch;
		onjump: (index: number) => void;
		onskip: () => void;
		onstop: () => void;
		label: (kind: string) => string;
	}
	let { batch, onjump, onskip, onstop, label }: Props = $props();

	let item = $derived(batch.current);
</script>

<div
	class="batch-bar"
	role="toolbar"
	aria-label="Importing several albums"
	transition:fly={{ y: -16, duration: 280, easing: cubicOut }}
>
	<BatchStrip {batch} {onjump} {label} />
	{#if item.status === 'importing'}
		<span class="note">Importing “{item.album.title}”…</span>
	{:else if item.status === 'imported'}
		<span class="note">Imported to {item.dest}</span>
	{/if}
	<div class="buttons">
		<button class="btn" onclick={onstop}>Stop</button>
		{#if item.open}<button class="btn" onclick={onskip}>Skip</button>{/if}
	</div>
</div>

<style>
	.batch-bar {
		position: fixed;
		top: 8px;
		left: 50%;
		/* Over the album sheet (30), under the import dialog (45). */
		z-index: 35;
		translate: -50% 0;
		width: max-content;
		max-width: min(1080px, calc(100vw - 32px));
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 6px 6px 6px 8px;
		border-radius: 999px;
		background: color-mix(in oklab, var(--bg-raised) 90%, transparent);
		backdrop-filter: blur(18px);
		border: 1px solid color-mix(in oklab, var(--accent) 45%, transparent);
		box-shadow: var(--shadow);
	}
	.btn {
		height: 32px;
		padding: 0 12px;
		font-size: 13px;
	}
	.note {
		min-width: 0;
		font-size: 13px;
		color: var(--text-2);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.buttons {
		flex: none;
		display: flex;
		gap: 6px;
		margin-left: auto;
	}
	/* Room above the sheet for the bar. */
	:global(body:has(.batch-bar) .sheet-wrap) {
		padding-top: 64px;
	}
	@media (max-width: 600px) {
		.batch-bar {
			top: 0;
			left: 0;
			right: 0;
			translate: none;
			width: auto;
			max-width: none;
			border-radius: 0;
			border-width: 0 0 1px;
			padding: 8px 12px;
		}
		:global(body:has(.batch-bar) .sheet-wrap) {
			padding-top: 52px;
		}
	}
</style>
