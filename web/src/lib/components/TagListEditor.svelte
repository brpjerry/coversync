<script lang="ts">
	import { flip } from 'svelte/animate';
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import ChevronUp from '@lucide/svelte/icons/chevron-up';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import X from '@lucide/svelte/icons/x';
	import Plus from '@lucide/svelte/icons/plus';
	import Check from '@lucide/svelte/icons/check';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import type { Sortable } from '#lib/api.ts';
	import { appearance, type TagList } from '#lib/appearance.svelte.ts';
	import { fold } from '#lib/format.ts';
	import { sortIcon } from '#lib/sort-icons.ts';
	import ShowAll from './ShowAll.svelte';

	interface Props {
		/** The list being edited. */
		list: TagList;
		/** Tags that can go on it. */
		choices: Sortable[];
		/** Filter text from the top bar's search field. */
		query: string;
		/** Where the list shows, e.g. "the top bar". */
		place: string;
		/** Shown while the list is empty. */
		empty?: string;
	}
	let { list, choices, query, place, empty }: Props = $props();

	/** Groups always on show; everything else sits behind "Show all tags". */
	const SHOWN = ['library', 'core'];

	let terms = $derived(fold(query).split(/\s+/).filter(Boolean));
	let groups = $derived(
		appearance.groups
			.map((g) => ({
				...g,
				items: choices.filter(
					(s) => s.group === g.id && terms.every((t) => fold(`${s.label} ${s.id}`).includes(t))
				)
			}))
			.filter((g) => g.items.length)
	);
	let otherGroups = $derived(groups.filter((g) => !SHOWN.includes(g.id)));
	/** Chosen tags from the hidden groups, flagged on the closed disclosure. */
	let hiddenChosen = $derived(
		list.items.filter((t) => !SHOWN.includes(choices.find((s) => s.id === t)?.group ?? '')).length
	);
</script>

<div class="layout">
	<div class="chosen">
		<h3>In {place}</h3>
		{#if list.items.length}
			<ol>
				{#each list.items as id, i (id)}
					{@const Icon = sortIcon(id)}
					<li animate:flip={{ duration: 300, easing: cubicOut }} in:fly={{ x: -10, duration: 240 }}>
						<span class="tab-icon"><Icon size={17} strokeWidth={1.8} /></span>
						<span class="tab-label">{appearance.label(id)}</span>
						<span class="tab-actions">
							<button
								class="mini"
								onclick={() => list.move(id, -1)}
								disabled={i === 0}
								aria-label="Move {appearance.label(id)} up"><ChevronUp size={16} /></button
							>
							<button
								class="mini"
								onclick={() => list.move(id, 1)}
								disabled={i === list.items.length - 1}
								aria-label="Move {appearance.label(id)} down"><ChevronDown size={16} /></button
							>
							<button
								class="mini remove"
								onclick={() => list.remove(id)}
								disabled={!list.removable(id)}
								title={list.removable(id) ? undefined : 'Keep at least one'}
								aria-label="Remove {appearance.label(id)}"><X size={16} /></button
							>
						</span>
					</li>
				{/each}
			</ol>
		{:else if empty}
			<p class="empty" in:fade={{ duration: 200 }}>{empty}</p>
		{/if}
		<button class="btn sm reset" onclick={() => list.reset()} disabled={list.isDefault}>
			<RotateCcw size={14} /> Reset to defaults
		</button>
	</div>

	<div class="available">
		<h3>Available</h3>
		{#snippet chipGroup(g: (typeof groups)[number])}
			<div class="group" transition:fade={{ duration: 150 }}>
				<span class="group-label">{g.label}</span>
				<div class="chips">
					{#each g.items as s (s.id)}
						{@const on = list.items.includes(s.id)}
						<button
							class="chip"
							class:on
							aria-pressed={on}
							onclick={() => list.toggle(s.id)}
							disabled={!list.removable(s.id)}
						>
							{#if on}<Check size={13} />{:else}<Plus size={13} />{/if}
							{s.label}
						</button>
					{/each}
				</div>
			</div>
		{/snippet}

		{#each groups.filter((g) => SHOWN.includes(g.id)) as g (g.id)}
			{@render chipGroup(g)}
		{/each}
		{#if otherGroups.length}
			<ShowAll
				label="Show all tags"
				count={otherGroups.reduce((n, g) => n + g.items.length, 0)}
				badge={hiddenChosen ? `${hiddenChosen} in ${place}` : undefined}
				forceOpen={terms.length > 0}
			>
				{#each otherGroups as g (g.id)}
					{@render chipGroup(g)}
				{/each}
			</ShowAll>
		{/if}
		{#if !groups.length}
			<p class="none">No tags match “{query}”.</p>
		{/if}
	</div>
</div>

<style>
	.layout {
		display: grid;
		grid-template-columns: minmax(240px, 300px) 1fr;
		gap: 28px;
		align-items: start;
	}
	h3 {
		margin: 0 0 10px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}

	.chosen {
		position: sticky;
		top: calc(var(--topbar-h) + 16px);
	}
	ol {
		list-style: none;
		margin: 0 0 12px;
		padding: 0;
		display: grid;
		gap: 6px;
	}
	li {
		display: flex;
		align-items: center;
		gap: 10px;
		height: 44px;
		padding: 0 6px 0 12px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.tab-icon {
		display: grid;
		color: var(--accent);
	}
	.tab-label {
		flex: 1;
		min-width: 0;
		font-size: 14px;
		font-weight: 540;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.tab-actions {
		display: flex;
		gap: 2px;
	}
	.mini {
		display: grid;
		place-items: center;
		width: 28px;
		height: 28px;
		border-radius: 8px;
		color: var(--text-3);
		transition:
			background 140ms,
			color 140ms;
	}
	.mini:hover:not(:disabled) {
		background: var(--surface-hover);
		color: var(--text);
	}
	.mini.remove:hover:not(:disabled) {
		color: var(--danger);
	}
	.mini:disabled {
		opacity: 0.3;
		cursor: default;
	}
	.empty {
		margin: 0 0 12px;
		padding: 12px 14px;
		border-radius: 12px;
		border: 1px dashed var(--border-strong);
		font-size: 13.5px;
		color: var(--text-3);
	}

	.available {
		display: grid;
		gap: 16px;
	}
	.available h3 {
		margin-bottom: -6px;
	}
	.group {
		display: grid;
		gap: 8px;
	}
	.group-label {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--text-2);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		height: 30px;
		padding: 0 11px 0 9px;
		border-radius: 999px;
		border: 1px solid var(--border);
		background: var(--surface);
		font-size: 13px;
		color: var(--text-2);
		transition:
			border-color 160ms,
			background 160ms,
			color 160ms,
			transform 160ms var(--ease-spring);
	}
	.chip:hover:not(:disabled) {
		border-color: var(--border-strong);
		color: var(--text);
	}
	.chip:active:not(:disabled) {
		transform: scale(0.95);
	}
	.chip.on {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 12%, var(--surface));
		color: var(--text);
	}
	.chip.on :global(svg) {
		color: var(--accent);
	}
	.chip:disabled {
		cursor: default;
	}
	.none {
		margin: 0;
		color: var(--text-3);
	}

	@media (max-width: 900px) {
		.layout {
			grid-template-columns: 1fr;
		}
		.chosen {
			position: static;
		}
	}
</style>
