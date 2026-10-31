<script lang="ts">
	import ArrowDownWideNarrow from '@lucide/svelte/icons/arrow-down-wide-narrow';
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
</script>

<button
	bind:this={button}
	class="icon-btn sort-btn"
	onclick={toggle}
	aria-label="Sort by {label(active)}"
	title="Sort by {label(active)}"
	aria-haspopup="menu"
	aria-expanded={!!menu}
	{disabled}
>
	<ArrowDownWideNarrow size={18} />
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
	.sort-btn[aria-expanded='true'] {
		color: var(--text);
		background: var(--surface-hover);
	}
	.sort-btn:disabled {
		opacity: 0.45;
		cursor: default;
		background: none;
		color: var(--text-2);
	}
</style>
