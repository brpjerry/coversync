<script lang="ts">
	import { fade, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import Search from '@lucide/svelte/icons/search';
	import X from '@lucide/svelte/icons/x';
	import Check from '@lucide/svelte/icons/check';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import FolderCog from '@lucide/svelte/icons/folder-cog';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Palette from '@lucide/svelte/icons/palette';
	import FolderSync from '@lucide/svelte/icons/folder-sync';
	import MonitorSmartphone from '@lucide/svelte/icons/monitor-smartphone';
	import type { Album } from '#lib/api.ts';
	import { isSetup, readPref, writePref, type View } from '#lib/browse.ts';
	import {
		isPicked,
		matcher,
		order,
		same,
		tagValues,
		toggled,
		type Filters,
		type TagValue
	} from '#lib/filters.ts';
	import { count, fold } from '#lib/format.ts';
	import { player } from '#lib/player.svelte.ts';
	import { sortIcon } from '#lib/sort-icons.ts';

	interface Props {
		/** Tags to filter by, in the dropdown's order (configured in Appearance). */
		kinds: string[];
		label: (id: string) => string;
		/** Albums the sort and search leave, before tag filters. */
		albums: Album[];
		/** Picked values by tag. */
		filters: Filters;
		collapsed: boolean;
		view: View;
		/** Sync is on: show the Sync section. */
		sync: boolean;
		/** Setup screens with unsaved changes. */
		dirty: Partial<Record<View, boolean>>;
		onview: (view: View) => void;
	}
	let {
		kinds,
		label,
		albums,
		filters = $bindable(),
		collapsed,
		view,
		sync,
		dirty,
		onview
	}: Props = $props();

	const setups = [
		{ id: 'inbox-setup', label: 'Inbox', icon: FolderCog },
		{ id: 'library-setup', label: 'Library', icon: Settings2 },
		{ id: 'appearance', label: 'Appearance', icon: Palette },
		{ id: 'sync-setup', label: 'Sync', icon: FolderSync }
	] as const;
	/** Filters only apply to album grids, not the setup screens. */
	let idle = $derived(isSetup(view));

	const KIND_KEY = 'coversync.filter';
	/** Values listed at first, and added by each "Show more": long lists are slow to draw. */
	const PAGE = 200;

	let chosen = $state(readPref(KIND_KEY, (v): v is string => typeof v === 'string', ''));
	/** The tag the dropdown shows: the one last chosen, or the first if Appearance removed it. */
	let kind = $derived(kinds.includes(chosen) ? chosen : kinds[0]);
	let find = $state('');
	let limit = $state(PAGE);

	function choose(k: string) {
		chosen = k;
		writePref(KIND_KEY, k);
		find = '';
		limit = PAGE;
	}

	/**
	 * Values of the tag on the albums the other tags' filters leave, so the
	 * counts say what picking one would show; picked values stay listed.
	 */
	let values = $derived.by((): TagValue[] => {
		if (!kind) return [];
		const list = tagValues(albums.filter(matcher(filters, kinds, kind)), kind);
		const gone = (filters[kind] ?? []).filter((v) => !list.some((t) => same(t.value, v)));
		return gone.length
			? [...list, ...gone.map((value) => ({ value, count: 0 }))].sort(order(kind))
			: list;
	});
	let terms = $derived(fold(find).split(/\s+/).filter(Boolean));
	let found = $derived(
		terms.length ? values.filter((t) => terms.every((w) => fold(t.value).includes(w))) : values
	);
	let shown = $derived(found.slice(0, limit));
	/** Every picked value, across tags. */
	let active = $derived(
		kinds.flatMap((k) => (filters[k] ?? []).map((value) => ({ kind: k, value })))
	);

	const pick = (k: string, value: string) => (filters = toggled(filters, k, value));
</script>

<aside
	id="sidebar"
	class="sidebar"
	class:collapsed
	aria-label="Filters and setup"
	inert={collapsed}
>
	<div class="inner" class:playing={!!player.current}>
		<div class="filter" class:idle inert={idle}>
			<div class="head">
				<h2 class="section">Filter</h2>
				{#if active.length}
					<button
						class="clear-all"
						onclick={() => (filters = {})}
						transition:fade={{ duration: 150 }}
					>
						Clear all
					</button>
				{/if}
			</div>

			{#if active.length}
				<ul
					class="picked"
					aria-label="Filtering by"
					transition:slide={{ duration: 220, easing: cubicOut }}
				>
					{#each active as p (`${p.kind}\n${p.value}`)}
						<li transition:fade={{ duration: 150 }}>
							<button
								class="chip"
								title="{label(p.kind)}: {p.value}"
								aria-label="Remove {label(p.kind)}: {p.value}"
								onclick={() => pick(p.kind, p.value)}
							>
								<span class="chip-text">{p.value}</span>
								<X size={12} strokeWidth={2.4} />
							</button>
						</li>
					{/each}
				</ul>
			{/if}

			{#if !kind}
				<div class="none">
					<p>No tags to filter by.</p>
					<button class="btn" onclick={() => onview('appearance')}
						><Palette size={14} /> Choose in Appearance</button
					>
				</div>
			{:else}
				{@const Icon = sortIcon(kind)}
				<label class="field kind">
					<span class="sr-only">Tag to filter by</span>
					<span class="lead" aria-hidden="true"><Icon size={16} strokeWidth={1.8} /></span>
					<select value={kind} onchange={(e) => choose(e.currentTarget.value)}>
						{#each kinds as k (k)}
							{@const n = filters[k]?.length ?? 0}
							<option value={k}>{label(k)}{n ? ` (${n})` : ''}</option>
						{/each}
					</select>
					<span class="chev" aria-hidden="true"><ChevronDown size={16} /></span>
				</label>

				<label class="field find">
					<span class="lead" aria-hidden="true"><Search size={15} /></span>
					<span class="sr-only">Search {label(kind)} values</span>
					<input
						type="search"
						bind:value={find}
						oninput={() => (limit = PAGE)}
						placeholder="Search {label(kind).toLowerCase()}"
						spellcheck="false"
						autocomplete="off"
					/>
					{#if find}
						<button
							class="clear"
							onclick={() => (find = '')}
							aria-label="Clear search"
							transition:fade={{ duration: 120 }}
						>
							<X size={13} />
						</button>
					{/if}
				</label>

				<div class="values" role="group" aria-label="{label(kind)} values">
					{#each shown as t (t.value)}
						{@const on = isPicked(filters, kind, t.value)}
						<button class="value" class:on aria-pressed={on} onclick={() => pick(kind, t.value)}>
							<span class="box" aria-hidden="true">
								{#if on}<span class="tick" in:fade={{ duration: 120 }}
										><Check size={12} strokeWidth={3} /></span
									>{/if}
							</span>
							<span class="name" title={t.value}>{t.value}</span>
							<span class="n tabular">{count(t.count)}</span>
						</button>
					{/each}
					{#if found.length > shown.length}
						<button class="more" onclick={() => (limit += PAGE)}>
							Show {count(Math.min(PAGE, found.length - shown.length))} more
						</button>
					{/if}
					{#if !values.length}
						<p class="empty">No {label(kind).toLowerCase()} on these albums.</p>
					{:else if !found.length}
						<p class="empty">No {label(kind).toLowerCase()} matches “{find}”.</p>
					{/if}
				</div>
			{/if}
		</div>

		{#if sync}
			<!-- A placeholder: the devices the Library syncs to will be listed here, each with its own settings. -->
			<section
				class="sync"
				aria-labelledby="sidebar-sync-title"
				transition:slide={{ duration: 220, easing: cubicOut }}
			>
				<h2 class="section" id="sidebar-sync-title">Sync</h2>
				<div class="devices">
					<span class="icon" aria-hidden="true"
						><MonitorSmartphone size={17} strokeWidth={1.8} /></span
					>
					<p>
						<b>No devices yet</b>
						Devices the Library syncs to will show up here, each with its own settings.
					</p>
				</div>
			</section>
		{/if}

		<nav class="setup" aria-label="Setup">
			<h2 class="section">Setup</h2>
			{#each setups as item (item.id)}
				<button
					class="nav-item"
					class:on={view === item.id}
					aria-current={view === item.id ? 'page' : undefined}
					onclick={() => onview(item.id)}
				>
					<span class="icon">
						<item.icon size={17} strokeWidth={1.8} />
						{#if dirty[item.id]}<i class="dot" aria-label="Unsaved changes"></i>{/if}
					</span>
					{item.label}
				</button>
			{/each}
		</nav>
	</div>
</aside>

<style>
	.sidebar {
		position: sticky;
		top: var(--topbar-h);
		align-self: start;
		height: calc(100dvh - var(--topbar-h));
		width: var(--sidebar-w);
		border-right: 1px solid var(--border);
		overflow: hidden;
		transition: width 380ms var(--ease-out);
	}
	.sidebar.collapsed {
		width: 0;
		border-right-width: 0;
	}
	/* Fixed width, so closing slides the panel away instead of squeezing it. */
	.inner {
		display: flex;
		flex-direction: column;
		width: var(--sidebar-w);
		height: 100%;
		padding: 18px 12px 0;
		transition: opacity 260ms;
	}
	.collapsed .inner {
		opacity: 0;
	}
	/* The tag filter takes the room the setup links leave, its value list scrolling;
	   in a window too short for that, the whole filter scrolls. It reaches the
	   sidebar's edges, so its scrollbar and the value list's sit there. */
	.filter {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
		gap: 10px;
		margin: 0 -12px;
		padding: 0 12px;
		overflow: hidden auto;
		scrollbar-width: thin;
		transition: opacity 240ms;
	}
	.filter.idle {
		opacity: 0.4;
	}

	.head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		height: 22px;
		padding: 0 4px 0 12px;
	}
	.section {
		margin: 0;
		font-size: 11.5px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-3);
	}
	.clear-all {
		padding: 2px 8px;
		border-radius: 999px;
		font-size: 12.5px;
		font-weight: 550;
		color: var(--accent);
		transition: background 140ms;
	}
	.clear-all:hover {
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}

	.picked {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		margin: 0;
		padding: 0 2px 4px;
		list-style: none;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		max-width: 100%;
		height: 26px;
		padding: 0 8px 0 10px;
		border-radius: 999px;
		font-size: 12.5px;
		font-weight: 540;
		color: var(--text);
		background: color-mix(in oklab, var(--accent) 16%, var(--surface));
		box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 34%, transparent);
		transition: background 140ms;
	}
	.chip :global(svg) {
		flex: none;
		color: var(--text-2);
	}
	.chip:hover {
		background: color-mix(in oklab, var(--accent) 26%, var(--surface));
	}
	.chip:hover :global(svg) {
		color: var(--text);
	}
	.chip-text {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.picked li {
		max-width: 100%;
	}

	.field {
		position: relative;
		display: flex;
		align-items: center;
		flex: none;
		height: 38px;
		border-radius: 11px;
		background: var(--surface);
		border: 1px solid var(--border);
		color: var(--text-3);
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.field:hover {
		border-color: var(--border-strong);
	}
	.field:focus-within {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.lead {
		position: absolute;
		left: 12px;
		display: grid;
		pointer-events: none;
	}
	.kind .lead {
		color: var(--accent);
	}
	select,
	input {
		width: 100%;
		height: 100%;
		min-width: 0;
		border: 0;
		outline: 0;
		background: none;
		color: var(--text);
		font: inherit;
		font-size: 14px;
	}
	select {
		appearance: none;
		padding: 0 34px 0 38px;
		font-weight: 560;
		text-overflow: ellipsis;
		cursor: pointer;
	}
	/* The native list takes the page's colours. */
	option {
		background: var(--bg-raised);
		color: var(--text);
	}
	.chev {
		position: absolute;
		right: 10px;
		display: grid;
		pointer-events: none;
	}
	input {
		padding: 0 34px 0 36px;
		font-size: 13.5px;
	}
	input::placeholder {
		color: var(--text-3);
	}
	input::-webkit-search-cancel-button {
		display: none;
	}
	.clear {
		position: absolute;
		right: 8px;
		display: grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 999px;
		background: var(--surface-hover);
		color: var(--text-2);
	}

	.values {
		flex: 1;
		/* About three values. */
		min-height: 120px;
		display: flex;
		flex-direction: column;
		gap: 1px;
		margin: 0 -12px;
		padding: 2px 12px 12px;
		overflow: hidden auto;
		scrollbar-width: thin;
	}
	.value {
		display: grid;
		grid-template-columns: 16px minmax(0, 1fr) auto;
		align-items: center;
		gap: 10px;
		flex: none;
		height: 34px;
		padding: 0 10px 0 12px;
		border-radius: 10px;
		text-align: left;
		font-size: 13.5px;
		color: var(--text-2);
		transition:
			background 140ms,
			color 140ms;
	}
	.value:hover {
		background: var(--surface);
		color: var(--text);
	}
	.value.on {
		color: var(--text);
		background: color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.value.on:hover {
		background: color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.box {
		display: grid;
		place-items: center;
		width: 16px;
		height: 16px;
		border-radius: 5px;
		border: 1.5px solid var(--border-strong);
		transition:
			background 160ms,
			border-color 160ms,
			transform 220ms var(--ease-spring);
	}
	.value:active .box {
		transform: scale(0.88);
	}
	.on .box {
		border-color: var(--accent);
		background: var(--accent);
		color: var(--accent-ink);
	}
	.tick {
		display: grid;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.n {
		font-size: 12px;
		color: var(--text-3);
	}
	.more {
		flex: none;
		height: 32px;
		margin-top: 4px;
		border-radius: 10px;
		font-size: 13px;
		font-weight: 550;
		color: var(--text-2);
	}
	.more:hover {
		background: var(--surface);
		color: var(--text);
	}
	.empty,
	.none p {
		margin: 6px 12px;
		font-size: 13px;
		color: var(--text-3);
	}
	.none {
		display: grid;
		justify-items: start;
		gap: 8px;
	}
	.none .btn {
		height: 32px;
		margin-left: 8px;
		padding: 0 12px;
		font-size: 13px;
	}

	.sync {
		flex: none;
		margin: 0 -12px;
		padding: 14px 12px;
		border-top: 1px solid var(--border);
	}
	.devices {
		display: grid;
		grid-template-columns: 16px minmax(0, 1fr);
		gap: 10px;
		padding: 0 12px;
		color: var(--text-3);
	}
	.devices .icon {
		height: 19px;
	}
	.devices p {
		margin: 0;
		font-size: 12.5px;
		line-height: 1.45;
	}
	.devices b {
		display: block;
		font-size: 13.5px;
		font-weight: 520;
		color: var(--text-2);
	}

	.setup {
		flex: none;
		display: grid;
		gap: 2px;
		margin: 0 -12px;
		padding: 14px 12px 16px;
		border-top: 1px solid var(--border);
		transition: padding-bottom 380ms var(--ease-out);
	}
	/* Clear of the player, which floats over the bottom of the window. */
	.playing .setup {
		padding-bottom: calc(var(--player-h) + 24px);
	}
	.sync .section,
	.setup .section {
		margin-bottom: 6px;
		padding-left: 12px;
	}
	.sync .section {
		margin-bottom: 10px;
	}
	.nav-item {
		display: grid;
		grid-template-columns: 16px minmax(0, 1fr);
		align-items: center;
		gap: 10px;
		height: 36px;
		padding: 0 12px;
		border-radius: 10px;
		text-align: left;
		font-size: 13.5px;
		font-weight: 520;
		color: var(--text-2);
		transition:
			color 160ms,
			background 140ms;
	}
	.nav-item:hover:not(.on) {
		background: var(--surface);
		color: var(--text);
	}
	.nav-item.on {
		color: var(--text);
		background: color-mix(in oklab, var(--accent) 14%, var(--surface));
		box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 28%, transparent);
	}
	.icon {
		position: relative;
		display: grid;
		place-items: center;
		transition: transform 300ms var(--ease-spring);
	}
	.nav-item.on .icon {
		color: var(--accent);
		transform: scale(1.08);
	}
	/* Unsaved changes on that screen. */
	.dot {
		position: absolute;
		top: -2px;
		right: -3px;
		width: 7px;
		height: 7px;
		border-radius: 999px;
		background: var(--accent);
		box-shadow: 0 0 0 2px var(--bg);
	}

	/* Narrow screens: an off-canvas drawer. */
	@media (max-width: 900px) {
		.sidebar {
			position: fixed;
			z-index: 26;
			top: 0;
			left: 0;
			height: 100dvh;
			width: 280px;
			background: var(--bg-raised);
			box-shadow: var(--shadow);
			transition: transform 380ms var(--ease-out);
		}
		.sidebar.collapsed {
			width: 280px;
			transform: translateX(-100%);
			box-shadow: none;
		}
		.inner {
			width: 280px;
			padding-top: 22px;
		}
		.collapsed .inner {
			opacity: 1;
		}
	}
</style>
