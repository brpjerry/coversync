<script lang="ts">
	import { fly } from 'svelte/transition';
	import ArrowDownUp from '@lucide/svelte/icons/arrow-down-up';
	import ListFilter from '@lucide/svelte/icons/list-filter';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LayoutGrid from '@lucide/svelte/icons/layout-grid';
	import Image from '@lucide/svelte/icons/image';
	import type { Album } from '#lib/api.ts';
	import { appearance } from '#lib/appearance.svelte.ts';
	import { gridLayout } from '#lib/grid.ts';
	import AlbumCard from './AlbumCard.svelte';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';
	import TagListEditor from './TagListEditor.svelte';

	interface Props {
		/** Filter text from the top bar's search field. */
		query: string;
		/** Saved: album data needs refreshing for any new sort tabs or filters. */
		onsaved: () => void;
		back: Back;
		/** The album grid's width, so the preview shows cards at the size they'll be. */
		gridWidth: number;
		/** Albums to preview the cover size with. */
		albums: Album[];
	}
	let { query, onsaved, back, gridWidth, albums }: Props = $props();

	appearance.load();

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
				<span class="card-icon"><ArrowDownUp size={20} /></span>
				<div>
					<h2 id="tabs-title">Sort tabs</h2>
					<p>
						The tags the top bar can sort albums by, in order. The top bar previews your changes as
						you make them.
					</p>
				</div>
			</header>
			<div class="card-body">
				<TagListEditor
					list={appearance.sorts}
					choices={appearance.sortable}
					{query}
					place="the top bar"
				/>
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="filters-title"
			in:fly={{ y: 12, duration: 420, delay: 120 }}
		>
			<header class="card-head">
				<span class="card-icon"><ListFilter size={20} /></span>
				<div>
					<h2 id="filters-title">Tag filters</h2>
					<p>
						The tags the sidebar can filter albums by, in the order its dropdown lists them. A tag
						with several values, split by Library setup's separator, offers each of them.
					</p>
				</div>
			</header>
			<div class="card-body">
				<TagListEditor
					list={appearance.filters}
					choices={appearance.filterable}
					{query}
					place="the sidebar"
					empty="None: the sidebar has nothing to filter by."
				/>
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
</style>
