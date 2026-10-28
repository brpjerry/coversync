<script lang="ts">
	import { flip } from 'svelte/animate';
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import PanelLeft from '@lucide/svelte/icons/panel-left';
	import ChevronUp from '@lucide/svelte/icons/chevron-up';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import X from '@lucide/svelte/icons/x';
	import Plus from '@lucide/svelte/icons/plus';
	import Check from '@lucide/svelte/icons/check';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LayoutGrid from '@lucide/svelte/icons/layout-grid';
	import Image from '@lucide/svelte/icons/image';
	import type { Album } from '#lib/api.ts';
	import { appearance } from '#lib/appearance.svelte.ts';
	import { fold } from '#lib/format.ts';
	import { gridLayout } from '#lib/grid.ts';
	import { sortIcon } from '#lib/sort-icons.ts';
	import AlbumCard from './AlbumCard.svelte';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import ShowAll from './ShowAll.svelte';
	import SaveBar from './SaveBar.svelte';

	interface Props {
		/** Filter text from the top bar's search field. */
		query: string;
		/** Saved: album data needs refreshing for any new sort tabs. */
		onsaved: () => void;
		back: Back;
		/** The album grid's width, so the preview shows cards at the size they'll be. */
		gridWidth: number;
		/** Albums to preview the cover size with. */
		albums: Album[];
	}
	let { query, onsaved, back, gridWidth, albums }: Props = $props();

	appearance.load();

	/** Groups always on show; everything else sits behind "Show all tags". */
	const SHOWN = ['library', 'core'];

	let terms = $derived(fold(query).split(/\s+/).filter(Boolean));
	let groups = $derived(
		appearance.groups
			.map((g) => ({
				...g,
				items: appearance.sortable.filter(
					(s) => s.group === g.id && terms.every((t) => fold(`${s.label} ${s.id}`).includes(t))
				)
			}))
			.filter((g) => g.items.length)
	);
	let otherGroups = $derived(groups.filter((g) => !SHOWN.includes(g.id)));
	/** Sidebar tabs from the hidden groups, flagged on the closed disclosure. */
	let hiddenTabs = $derived(
		appearance.tabs.filter(
			(t) => !SHOWN.includes(appearance.sortable.find((s) => s.id === t)?.group ?? '')
		).length
	);

	let sizes = $derived(appearance.coverSizes);
	let grid = $derived(gridLayout(gridWidth, appearance.coverSize));
	/** How far along the slider the size is, 0 to 1, for its filled track. */
	let fraction = $derived((appearance.coverSize - sizes.min) / (sizes.max - sizes.min));
	let perRow = $derived(`${grid.cols} per row`);
	const noop = () => {};
</script>

<SetupPane title="Appearance" description="How the album browser looks." {back}>
	{#if appearance.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the appearance settings from coversyncd.
			<button class="btn sm" onclick={() => appearance.load()}>Retry</button>
		</div>
	{:else if !appearance.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="size-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><LayoutGrid size={20} /></span>
				<div>
					<h2 id="size-title">Cover art size</h2>
					<p>
						How big album covers are in the Inbox and Library. Each row's covers stretch a little to
						fill the window.
					</p>
				</div>
			</header>
			<div class="card-body sizing">
				<div class="size-row">
					<span class="end" aria-hidden="true"><Image size={14} /></span>
					<input
						class="slider"
						type="range"
						min={sizes.min}
						max={sizes.max}
						step="4"
						bind:value={appearance.coverSize}
						style:--v={fraction}
						aria-label="Cover art size"
						aria-valuetext="{appearance.coverSize} pixels, {perRow}"
					/>
					<span class="end" aria-hidden="true"><Image size={22} /></span>
					<output class="readout" aria-live="polite">
						{#key grid.cols}
							<b in:fly={{ y: 6, duration: 220 }}>{perRow}</b>
						{/key}
						<span>in this window, {Math.round(grid.cellWidth)} px wide</span>
					</output>
					<button
						class="btn sm"
						onclick={() => (appearance.coverSize = sizes.default)}
						disabled={appearance.coverSize === sizes.default}
					>
						<RotateCcw size={14} /> Default
					</button>
				</div>
				{#if albums.length}
					<div class="cover-preview" inert style:gap="{grid.colGap}px">
						{#each albums as album (album.id)}
							<div class="cell" style:width="{grid.cellWidth}px">
								<AlbumCard
									{album}
									width={grid.cellWidth}
									onopen={noop}
									onplay={noop}
									onmenu={noop}
								/>
							</div>
						{/each}
					</div>
				{/if}
			</div>
		</section>

		<section class="card" aria-labelledby="tabs-title" in:fly={{ y: 12, duration: 420, delay: 60 }}>
			<header class="card-head">
				<span class="card-icon"><PanelLeft size={20} /></span>
				<div>
					<h2 id="tabs-title">Sidebar sort tabs</h2>
					<p>
						The tags the sidebar can sort albums by, top to bottom. The sidebar previews your
						changes as you make them.
					</p>
				</div>
			</header>
			<div class="card-body layout">
				<div class="chosen">
					<h3>In the sidebar</h3>
					<ol>
						{#each appearance.tabs as id, i (id)}
							{@const Icon = sortIcon(id)}
							<li
								animate:flip={{ duration: 300, easing: cubicOut }}
								in:fly={{ x: -10, duration: 240 }}
							>
								<span class="tab-icon"><Icon size={17} strokeWidth={1.8} /></span>
								<span class="tab-label">{appearance.label(id)}</span>
								<span class="tab-actions">
									<button
										class="mini"
										onclick={() => appearance.move(id, -1)}
										disabled={i === 0}
										aria-label="Move {appearance.label(id)} up"><ChevronUp size={16} /></button
									>
									<button
										class="mini"
										onclick={() => appearance.move(id, 1)}
										disabled={i === appearance.tabs.length - 1}
										aria-label="Move {appearance.label(id)} down"><ChevronDown size={16} /></button
									>
									<button
										class="mini remove"
										onclick={() => appearance.remove(id)}
										disabled={appearance.tabs.length === 1}
										title={appearance.tabs.length === 1 ? 'Keep at least one tab' : undefined}
										aria-label="Remove {appearance.label(id)}"><X size={16} /></button
									>
								</span>
							</li>
						{/each}
					</ol>
					<button
						class="btn sm reset"
						onclick={() => appearance.reset()}
						disabled={appearance.isDefault}
					>
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
									{@const on = appearance.tabs.includes(s.id)}
									<button
										class="chip"
										class:on
										aria-pressed={on}
										onclick={() => appearance.toggle(s.id)}
										disabled={on && appearance.tabs.length === 1}
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
							badge={hiddenTabs ? `${hiddenTabs} in the sidebar` : undefined}
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
		</section>

		<SaveBar
			changes={appearance.changes}
			saving={appearance.saving}
			saveLabel="Save"
			onrevert={() => appearance.revert()}
			onsave={async () => {
				if (await appearance.save()) onsaved();
			}}
		/>
	{/if}
</SetupPane>

<style>
	.sizing {
		display: grid;
		gap: 20px;
	}
	.size-row {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.end {
		display: grid;
		place-items: center;
		width: 22px;
		color: var(--text-3);
	}
	.slider {
		flex: 1 1 220px;
		min-width: 160px;
		max-width: 420px;
		height: 24px;
		appearance: none;
		background: transparent;
		cursor: pointer;
	}
	.slider:focus-visible {
		outline: none;
	}
	.slider::-moz-range-track {
		height: 6px;
		border-radius: 999px;
		background: var(--border-strong);
	}
	.slider::-moz-range-progress {
		height: 6px;
		border-radius: 999px;
		background: var(--accent);
	}
	.slider::-moz-range-thumb {
		width: 18px;
		height: 18px;
		border: 0;
		border-radius: 999px;
		background: var(--text);
		box-shadow: 0 2px 8px rgb(0 0 0 / 0.4);
		transition: transform 200ms var(--ease-spring);
	}
	.slider::-webkit-slider-runnable-track {
		height: 6px;
		border-radius: 999px;
		background: linear-gradient(
			to right,
			var(--accent) calc(var(--v) * 100%),
			var(--border-strong) 0
		);
	}
	.slider::-webkit-slider-thumb {
		appearance: none;
		width: 18px;
		height: 18px;
		margin-top: -6px;
		border-radius: 999px;
		background: var(--text);
		box-shadow: 0 2px 8px rgb(0 0 0 / 0.4);
		transition: transform 200ms var(--ease-spring);
	}
	.slider:active::-moz-range-thumb {
		transform: scale(1.15);
	}
	.slider:active::-webkit-slider-thumb {
		transform: scale(1.15);
	}
	.slider:focus-visible::-moz-range-thumb {
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 40%, transparent);
	}
	.slider:focus-visible::-webkit-slider-thumb {
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 40%, transparent);
	}
	.readout {
		display: grid;
		min-width: 0;
		margin: 0 auto 0 8px;
		line-height: 1.3;
	}
	.readout b {
		font-size: 15px;
		font-weight: 620;
		font-variant-numeric: tabular-nums;
	}
	.readout span {
		font-size: 12.5px;
		color: var(--text-3);
		font-variant-numeric: tabular-nums;
	}
	.cover-preview {
		display: flex;
		align-items: flex-start;
		overflow: hidden;
		/* Room for the cards' hover lift and shadow, which the clip would cut. */
		padding: 6px 2px 2px;
		margin: -6px -2px -2px;
		mask-image: linear-gradient(to right, #000 calc(100% - 96px), transparent);
	}
	.cell {
		flex: none;
		transition: width 320ms var(--ease-out);
	}

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
