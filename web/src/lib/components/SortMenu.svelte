<script lang="ts">
	import ArrowDownWideNarrow from '@lucide/svelte/icons/arrow-down-wide-narrow';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import type { Sort } from '#lib/browse.ts';
	import { sortIcon } from '#lib/sort-icons.ts';
	import ContextMenu, { type MenuEntry } from './ContextMenu.svelte';

	interface Props {
		/** Sorts to offer, in order (configured in Appearance). */
		sorts: Sort[];
		/** The sort in use. */
		active: Sort;
		label: (id: Sort) => string;
		onsort: (id: Sort) => void;
		/** Counts shown beside sorts, e.g. how many albums need attention. */
		counts?: Partial<Record<Sort, number>>;
		/** Sorting doesn't apply here (setup screens). */
		disabled: boolean;
	}
	let { sorts, active, label, onsort, counts = {}, disabled }: Props = $props();

	let button = $state<HTMLButtonElement>();
	/** Where the menu opens, while it's open. */
	let menu = $state<{ x: number; y: number } | null>(null);
	let items = $derived(
		sorts.map((id): MenuEntry => ({
			label: label(id),
			icon: sortIcon(id),
			run: () => onsort(id),
			checked: active === id,
			count: counts[id],
			warn: id === 'attention'
		}))
	);

	function toggle() {
		if (menu || !button) return (menu = null);
		// Below the button, its right edge lined up with the button's.
		const r = button.getBoundingClientRect();
		menu = { x: r.right, y: r.bottom + 8 };
	}
	let Icon = $derived(sortIcon(active));
</script>

<!-- Names the sort in use; phones get the sort glyph alone, the menu says the rest. -->
<button
	bind:this={button}
	class="sort-btn"
	class:warn={active === 'attention'}
	onclick={toggle}
	aria-label="Sort by {label(active)}"
	aria-haspopup="menu"
	aria-expanded={!!menu}
	{disabled}
>
	<span class="glyph" aria-hidden="true"><ArrowDownWideNarrow size={17} /></span>
	<span class="active" aria-hidden="true"><Icon size={15} strokeWidth={2} /></span>
	<span class="label">{label(active)}</span>
	<span class="chev" aria-hidden="true"><ChevronDown size={14} /></span>
</button>

{#if menu}
	<ContextMenu
		x={menu.x}
		y={menu.y}
		title="Sort by"
		{items}
		anchor={button}
		onclose={() => (menu = null)}
	/>
{/if}

<style>
	.sort-btn {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		height: 36px;
		padding: 0 9px 0 11px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-size: 13.5px;
		font-weight: 560;
		color: var(--text-2);
		white-space: nowrap;
		transition:
			background 160ms,
			border-color 160ms,
			color 160ms,
			opacity 160ms;
	}
	.sort-btn:hover:not(:disabled),
	.sort-btn[aria-expanded='true'] {
		background: var(--surface-hover);
		border-color: var(--border-strong);
		color: var(--text);
	}
	.sort-btn:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.active {
		display: grid;
		color: var(--accent);
	}
	.warn .active {
		color: var(--warn);
	}
	.chev {
		display: grid;
		color: var(--text-3);
	}
	/* The sort glyph stands in for the label on phones. */
	.glyph {
		display: none;
	}
	@media (max-width: 600px) {
		.sort-btn {
			width: 36px;
			padding: 0;
			justify-content: center;
		}
		.glyph {
			display: grid;
		}
		.active,
		.label,
		.chev {
			display: none;
		}
	}
</style>
