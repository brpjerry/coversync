<script lang="ts">
	import { fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import type { ImportBatch } from '#lib/batch.svelte.ts';
	import BatchStrip from './BatchStrip.svelte';

	// The batch's strip, Skip and Stop while the album's dialog waits for its plan.

	interface Props {
		batch: ImportBatch;
		onjump: (index: number) => void;
		onskip: () => void;
		onstop: () => void;
		label: (kind: string) => string;
	}
	let { batch, onjump, onskip, onstop, label }: Props = $props();
</script>

<div
	class="batch-bar"
	role="toolbar"
	aria-label="Importing several albums"
	transition:fly={{ y: -16, duration: 280, easing: cubicOut }}
>
	<BatchStrip {batch} {onjump} {label} />
	<div class="buttons">
		<button class="btn" onclick={onstop}>Stop</button>
		<button class="btn" onclick={onskip}>Skip</button>
	</div>
</div>

<style>
	.batch-bar {
		position: fixed;
		top: 8px;
		left: 50%;
		/* Over the grid, under the import dialog (45). */
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
	.buttons {
		flex: none;
		display: flex;
		gap: 6px;
		margin-left: auto;
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
	}
</style>
