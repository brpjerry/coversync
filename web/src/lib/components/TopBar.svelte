<script lang="ts">
	import Search from '@lucide/svelte/icons/search';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import X from '@lucide/svelte/icons/x';
	import Inbox from '@lucide/svelte/icons/inbox';
	import LibraryBig from '@lucide/svelte/icons/library-big';
	import PanelLeftClose from '@lucide/svelte/icons/panel-left-close';
	import PanelLeftOpen from '@lucide/svelte/icons/panel-left-open';
	import { fade, fly } from 'svelte/transition';
	import type { ScanProgress } from '#lib/api.ts';
	import { collection, type View } from '#lib/browse.ts';
	import { count } from '#lib/format.ts';

	interface Props {
		query: string;
		view: View;
		/** Album counts per collection; null while still loading. */
		counts: Record<'inbox' | 'library', number | null>;
		scan: ScanProgress | null;
		shown: number;
		total: number;
		sidebarOpen: boolean;
		onview: (view: View) => void;
		ontogglesidebar: () => void;
		onrescan: () => void;
		search?: HTMLInputElement;
	}
	let {
		query = $bindable(),
		view,
		counts,
		scan,
		shown,
		total,
		sidebarOpen,
		onview,
		ontogglesidebar,
		onrescan,
		search = $bindable()
	}: Props = $props();

	const views = [
		{ id: 'inbox', label: 'Inbox', icon: Inbox },
		{ id: 'library', label: 'Library', icon: LibraryBig }
	] as const;

	// Each setup screen sits under its collection's tab.
	let tab = $derived(collection(view));
	/** Library setup and Appearance reuse the search field to filter tags. */
	let filtering = $derived(view === 'library-setup' || view === 'appearance');
	/** Inbox setup has nothing to search. */
	let searchable = $derived(view !== 'inbox-setup');

	let pct = $derived.by(() => {
		if (!scan?.running) return 0;
		if (scan.phase === 'covers')
			return scan.covers_total ? scan.covers_done / scan.covers_total : 1;
		return scan.files_seen ? scan.files_read / scan.files_seen : 0;
	});
	let label = $derived.by(() => {
		if (!scan?.running) return '';
		if (scan.phase === 'walking') return `Finding files · ${count(scan.files_seen)}`;
		if (scan.phase === 'reading')
			return `Reading tags · ${count(scan.files_read)} / ${count(scan.files_seen)}`;
		return `Covers · ${count(scan.covers_done)} / ${count(scan.covers_total)}`;
	});
</script>

<header class="bar">
	<div class="lead">
		<button
			class="icon-btn toggle"
			onclick={ontogglesidebar}
			aria-label={sidebarOpen ? 'Collapse sidebar' : 'Expand sidebar'}
			aria-expanded={sidebarOpen}
			aria-controls="sidebar"
		>
			{#if sidebarOpen}<PanelLeftClose size={19} />{:else}<PanelLeftOpen size={19} />{/if}
		</button>
		<div class="brand">
			<span class="logo" aria-hidden="true"></span>
			<span class="name">coversync</span>
		</div>
	</div>

	<label class="search" class:disabled={!searchable}>
		<Search size={16} />
		<span class="sr-only">{filtering ? 'Filter tags' : 'Search albums'}</span>
		<input
			bind:this={search}
			bind:value={query}
			type="search"
			placeholder={filtering ? 'Filter tags' : 'Search albums, artists, genres'}
			disabled={!searchable}
			spellcheck="false"
			autocomplete="off"
		/>
		{#if query}
			<button
				class="clear"
				onclick={() => (query = '')}
				aria-label="Clear search"
				transition:fade={{ duration: 120 }}
			>
				<X size={14} />
			</button>
		{:else}
			<kbd>/</kbd>
		{/if}
	</label>

	<div
		class="tabs"
		role="tablist"
		aria-label="Collection"
		style:--index={tab === 'library' ? 1 : 0}
		class:none={tab === null}
	>
		<span class="thumb" aria-hidden="true"></span>
		{#each views as v (v.id)}
			<button
				role="tab"
				aria-selected={tab === v.id}
				class:on={tab === v.id}
				onclick={() => onview(v.id)}
			>
				<v.icon size={15} strokeWidth={1.9} />
				{v.label}
				{#if counts[v.id] !== null}
					<span class="badge tabular" in:fade={{ duration: 200 }}>{count(counts[v.id]!)}</span>
				{/if}
			</button>
		{/each}
	</div>

	<div class="right">
		{#if scan?.running}
			<div class="scan" transition:fly={{ y: -6, duration: 240 }} title={label}>
				<span class="ring" style:--p={pct}></span>
				<span class="scan-label tabular">{label}</span>
			</div>
		{:else if !filtering && searchable}
			<span class="stats tabular" in:fade>
				{shown !== total ? `${count(shown)} of ${count(total)}` : count(total)} albums
			</span>
		{/if}
		<button
			class="icon-btn"
			onclick={onrescan}
			aria-label="Rescan library"
			disabled={scan?.running}
		>
			<span class:spin={scan?.running}><RefreshCw size={17} /></span>
		</button>
	</div>
</header>

<style>
	.bar {
		position: sticky;
		top: 0;
		z-index: 20;
		display: grid;
		grid-template-columns: auto minmax(180px, 440px) auto 1fr;
		align-items: center;
		gap: 20px;
		height: var(--topbar-h);
		/* The toggle lines up with the sidebar rail's icons. */
		padding: 0 28px 0 16px;
		background: var(--bg-overlay);
		backdrop-filter: saturate(1.6) blur(20px);
		border-bottom: 1px solid var(--border);
	}
	.lead {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.toggle {
		flex: none;
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		font-weight: 680;
		letter-spacing: -0.02em;
		font-size: 17px;
	}
	.logo {
		width: 22px;
		height: 22px;
		border-radius: 7px;
		background:
			radial-gradient(circle at 50% 50%, var(--bg) 0 3px, transparent 3.5px),
			conic-gradient(
				from 200deg,
				hsl(252 85% 74%),
				hsl(320 80% 70%),
				hsl(30 90% 66%),
				hsl(252 85% 74%)
			);
		box-shadow: 0 4px 14px -4px hsl(280 80% 60% / 0.6);
	}

	.search {
		display: flex;
		align-items: center;
		gap: 10px;
		height: 40px;
		padding: 0 12px 0 14px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
		color: var(--text-3);
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.search.disabled {
		opacity: 0.45;
	}
	.search:focus-within {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.search input {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: 0;
		background: none;
		color: var(--text);
	}
	.search input::-webkit-search-cancel-button {
		display: none;
	}
	kbd {
		font: inherit;
		font-size: 12px;
		padding: 1px 7px;
		border-radius: 6px;
		border: 1px solid var(--border-strong);
		color: var(--text-3);
	}
	.clear {
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border-radius: 999px;
		background: var(--surface-hover);
		color: var(--text-2);
	}

	.tabs {
		position: relative;
		display: grid;
		grid-template-columns: 1fr 1fr;
		padding: 3px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.tabs button {
		position: relative;
		z-index: 1;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 7px;
		height: 32px;
		padding: 0 14px;
		border-radius: 999px;
		font-size: 13.5px;
		font-weight: 560;
		color: var(--text-2);
		white-space: nowrap;
		transition: color 200ms;
	}
	.tabs button:hover:not(.on) {
		color: var(--text);
	}
	.tabs button.on {
		color: var(--text);
	}
	.badge {
		min-width: 20px;
		padding: 1px 7px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		background: var(--surface-hover);
		color: var(--text-3);
		transition:
			background 240ms,
			color 240ms;
	}
	.on .badge {
		background: color-mix(in oklab, var(--accent) 22%, transparent);
		color: var(--accent);
	}
	.none .thumb {
		opacity: 0;
	}
	.thumb {
		position: absolute;
		top: 3px;
		bottom: 3px;
		left: 3px;
		width: calc(50% - 3px);
		border-radius: 999px;
		background: var(--surface-hover);
		box-shadow: inset 0 0 0 1px var(--border-strong);
		transform: translateX(calc(var(--index) * 100%));
		transition: transform 420ms var(--ease-spring);
	}

	.right {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: 12px;
		min-width: 0;
	}
	.stats {
		color: var(--text-3);
		font-size: 13px;
		white-space: nowrap;
	}
	.scan {
		display: flex;
		align-items: center;
		gap: 10px;
		height: 32px;
		padding: 0 12px 0 8px;
		border-radius: 999px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-size: 12.5px;
		color: var(--text-2);
		min-width: 0;
	}
	.scan-label {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.ring {
		flex: none;
		width: 16px;
		height: 16px;
		border-radius: 999px;
		background:
			radial-gradient(closest-side, var(--bg-raised) 64%, transparent 66%),
			conic-gradient(var(--accent) calc(var(--p) * 360deg), var(--border-strong) 0);
	}
	.spin {
		display: inline-grid;
		animation: spin 1.1s linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}

	@media (max-width: 900px) {
		.bar {
			grid-template-columns: auto 1fr auto;
			height: auto;
			padding: 12px 16px;
			row-gap: 12px;
		}
		.tabs {
			grid-row: 2;
			grid-column: 1 / -1;
		}
		.brand .name {
			display: none;
		}
		.stats,
		.scan-label {
			display: none;
		}
	}
</style>
