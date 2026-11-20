<script lang="ts">
	import X from '@lucide/svelte/icons/x';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import { fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import { count } from '#lib/format.ts';

	interface Props {
		/** Albums selected, those the search and filters hide included. */
		selected: number;
		/** Selected albums the search and filters hide. */
		hidden: number;
		/** Albums shown; Select all selects these. */
		shown: number;
		/** Every album shown is selected. */
		allShown: boolean;
		onclose: () => void;
		onall: () => void;
		onclear: () => void;
		onimport: () => void;
	}
	let { selected, hidden, shown, allShown, onclose, onall, onclear, onimport }: Props = $props();
</script>

<div
	class="selection"
	role="toolbar"
	aria-label="Selected albums"
	transition:fly={{ y: 24, duration: 320, easing: cubicOut }}
>
	<button
		class="icon-btn"
		onclick={onclose}
		aria-label="Stop selecting"
		title="Stop selecting (Esc)"
	>
		<X size={18} />
	</button>
	<span class="summary tabular" aria-live="polite">
		{#key selected}<b in:fly={{ y: 6, duration: 200 }}>{count(selected)}</b>{/key} selected
		{#if hidden}<span class="hidden"> · {count(hidden)} hidden by the search or filters</span>{/if}
	</span>
	<div class="buttons">
		{#if !allShown && shown}
			<button class="link" onclick={onall}>Select all {count(shown)}</button>
		{/if}
		{#if selected}
			<button class="link" onclick={onclear}>Clear</button>
		{/if}
		<button class="btn primary" onclick={onimport} disabled={!selected}>
			<FolderInput size={15} /> Import{selected ? ` ${count(selected)}` : ''}…
		</button>
	</div>
</div>

<style>
	.selection {
		position: fixed;
		left: 50%;
		/* Just above the player while it's showing (--dock). */
		bottom: calc(var(--dock, 0px) + 20px);
		/* Over the grid and the player (40), under the album sheet and dialogs. */
		z-index: 41;
		translate: -50% 0;
		width: max-content;
		max-width: calc(100vw - 32px);
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 8px 8px 8px 8px;
		border-radius: 999px;
		background: color-mix(in oklab, var(--bg-raised) 88%, transparent);
		backdrop-filter: blur(18px);
		border: 1px solid color-mix(in oklab, var(--accent) 45%, transparent);
		box-shadow:
			var(--shadow),
			0 0 0 4px color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.summary {
		font-size: 13.5px;
		color: var(--text-2);
		white-space: nowrap;
	}
	.summary b {
		display: inline-block;
		color: var(--accent);
	}
	.hidden {
		color: var(--text-3);
	}
	.buttons {
		display: flex;
		align-items: center;
		gap: 12px;
		margin-left: 8px;
	}
	.link {
		font-size: 13.5px;
		font-weight: 600;
		color: var(--accent);
		white-space: nowrap;
	}
	.link:hover {
		text-decoration: underline;
	}
	/* Phones: along the bottom edge, above the player. */
	@media (max-width: 600px) {
		.selection {
			left: 0;
			right: 0;
			bottom: var(--dock, 0px);
			translate: none;
			width: auto;
			max-width: none;
			flex-wrap: wrap;
			gap: 4px 10px;
			padding: 8px 12px;
			border-radius: 0;
			border-width: 1px 0 0;
			box-shadow: 0 -12px 32px -16px rgb(0 0 0 / 0.5);
		}
		.summary {
			flex: 1;
			white-space: normal;
		}
		.buttons {
			margin-left: auto;
		}
	}
</style>
