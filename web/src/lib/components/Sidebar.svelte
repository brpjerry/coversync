<script lang="ts">
	import { flip } from 'svelte/animate';
	import { fade } from 'svelte/transition';
	import FolderCog from '@lucide/svelte/icons/folder-cog';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Palette from '@lucide/svelte/icons/palette';
	import { isSetup, type Sort, type View } from '#lib/browse.ts';
	import { sortIcon } from '#lib/sort-icons.ts';

	interface Props {
		/** Sort tabs to show, in order (configured in Appearance). */
		tabs: Sort[];
		/** The sort in use. */
		active: Sort;
		label: (id: Sort) => string;
		onsort: (id: Sort) => void;
		/** Counts shown beside tabs, e.g. how many albums need attention. */
		counts?: Partial<Record<Sort, number>>;
		view: View;
		collapsed: boolean;
		/** Collapsed into an off-canvas drawer (narrow screens): keep it out of the tab order. */
		offscreen: boolean;
		/** Setup screens with unsaved changes. */
		dirty: Partial<Record<View, boolean>>;
		onview: (view: View) => void;
	}
	let {
		tabs,
		active,
		label,
		onsort,
		counts = {},
		view,
		collapsed,
		offscreen,
		dirty,
		onview
	}: Props = $props();

	const setups = [
		{ id: 'inbox-setup', label: 'Inbox', title: 'Inbox setup', icon: FolderCog },
		{ id: 'library-setup', label: 'Library', title: 'Library setup', icon: Settings2 },
		{ id: 'appearance', label: 'Appearance', title: 'Appearance', icon: Palette }
	] as const;

	let index = $derived(Math.max(0, tabs.indexOf(active)));
	/** On the Appearance screen the tabs are a live preview of the edits. */
	let previewing = $derived(view === 'appearance');
</script>

<aside id="sidebar" class="sidebar" class:collapsed aria-label="Browse" inert={offscreen}>
	<nav>
		<!-- Sorting only applies to album grids, not the setup screens. -->
		<div class="block" class:idle={isSetup(view) && !previewing} inert={isSetup(view)}>
			<h2 class="section">
				<span
					>Sort by{#if previewing}<em class="preview" transition:fade>{' · preview'}</em>{/if}</span
				>
			</h2>
			<div class="list" role="radiogroup" aria-label="Sort albums by" style:--index={index}>
				<span class="pill" aria-hidden="true"></span>
				{#each tabs as id (id)}
					{@const Icon = sortIcon(id)}
					<button
						role="radio"
						aria-checked={active === id}
						class:on={active === id}
						title={collapsed ? label(id) : undefined}
						onclick={() => onsort(id)}
						animate:flip={{ duration: 300 }}
						transition:fade={{ duration: 180 }}
					>
						<span class="icon">
							<Icon size={18} strokeWidth={1.8} />
							{#if counts[id]}<i class="dot warn" aria-hidden="true"></i>{/if}
						</span>
						<span class="label">{label(id)}</span>
						{#if counts[id]}
							<span
								class="tab-count tabular"
								class:warn={id === 'attention'}
								transition:fade={{ duration: 160 }}>{counts[id]}</span
							>
						{/if}
					</button>
				{/each}
			</div>
		</div>

		<div class="block">
			<h2 class="section"><span>Setup</span></h2>
			<div class="list">
				{#each setups as item (item.id)}
					<button
						class="nav-item"
						class:on={view === item.id}
						aria-current={view === item.id ? 'page' : undefined}
						title={collapsed ? item.title : undefined}
						onclick={() => onview(item.id)}
					>
						<span class="icon">
							<item.icon size={18} strokeWidth={1.8} />
							{#if dirty[item.id]}<i class="dot" aria-label="Unsaved changes"></i>{/if}
						</span>
						<span class="label">{item.label}</span>
					</button>
				{/each}
			</div>
		</div>
	</nav>
</aside>

<style>
	.sidebar {
		--item-h: 40px;
		--item-gap: 4px;
		position: sticky;
		top: var(--topbar-h);
		align-self: start;
		height: calc(100dvh - var(--topbar-h));
		width: var(--sidebar-w);
		padding: 20px 12px calc(var(--player-h) + 24px);
		border-right: 1px solid var(--border);
		/* Many sort tabs can outgrow the window. */
		overflow: hidden auto;
		scrollbar-width: thin;
		transition: width 380ms var(--ease-out);
	}
	.sidebar.collapsed {
		width: var(--rail-w);
	}

	nav {
		display: grid;
		gap: 26px;
	}
	.block {
		transition: opacity 240ms;
	}
	.block.idle {
		opacity: 0.4;
	}
	.nav-item.on {
		background: color-mix(in oklab, var(--accent) 14%, var(--surface));
		box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 28%, transparent);
	}
	.icon {
		position: relative;
	}
	.tab-count {
		margin-left: auto;
		padding: 0 7px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-3);
		background: var(--surface-hover);
		transition: opacity 220ms;
	}
	.tab-count.warn {
		color: hsl(40 95% 70%);
		background: hsl(40 90% 55% / 0.16);
	}
	.collapsed .tab-count {
		opacity: 0;
	}
	/* In the rail the count becomes a dot on the icon. */
	.dot.warn {
		background: hsl(40 95% 62%);
		opacity: 0;
		transition: opacity 220ms;
	}
	.collapsed .dot.warn {
		opacity: 1;
	}
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
	.section {
		position: relative;
		height: 20px;
		margin: 0 0 10px;
		padding: 0 12px;
		font-size: 11.5px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-3);
		white-space: nowrap;
	}
	.section span {
		transition: opacity 200ms;
	}
	.preview {
		font-style: normal;
		color: var(--accent);
	}
	/* In the rail, the heading shrinks into a short rule. */
	.section::after {
		content: '';
		position: absolute;
		left: 12px;
		top: 50%;
		width: 20px;
		height: 1px;
		background: var(--border-strong);
		opacity: 0;
		transition: opacity 200ms;
	}
	.collapsed .section span {
		opacity: 0;
	}
	.collapsed .section::after {
		opacity: 1;
		transition-delay: 120ms;
	}

	.list {
		position: relative;
		display: grid;
		gap: var(--item-gap);
	}
	.pill {
		position: absolute;
		inset: 0 0 auto;
		height: var(--item-h);
		border-radius: 12px;
		background: color-mix(in oklab, var(--accent) 14%, var(--surface));
		box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 28%, transparent);
		transform: translateY(calc(var(--index) * (var(--item-h) + var(--item-gap))));
		transition: transform 420ms var(--ease-spring);
	}
	button {
		position: relative;
		display: grid;
		grid-template-columns: 20px minmax(0, 1fr) auto;
		align-items: center;
		gap: 14px;
		height: var(--item-h);
		padding: 0 12px;
		border-radius: 12px;
		text-align: left;
		font-size: 14px;
		font-weight: 520;
		color: var(--text-2);
		white-space: nowrap;
		transition:
			color 200ms,
			background 160ms;
	}
	button:hover:not(.on) {
		background: var(--surface);
		color: var(--text);
	}
	button.on {
		color: var(--text);
	}
	.icon {
		display: grid;
		place-items: center;
		transition: transform 300ms var(--ease-spring);
	}
	button.on .icon {
		color: var(--accent);
		transform: scale(1.08);
	}
	.label {
		overflow: hidden;
		text-overflow: ellipsis;
		transition:
			opacity 220ms,
			transform 300ms var(--ease-out);
	}
	.collapsed .label {
		opacity: 0;
		transform: translateX(-6px);
	}

	/* Narrow screens: an off-canvas drawer instead of a rail. */
	@media (max-width: 900px) {
		.sidebar {
			position: fixed;
			z-index: 26;
			top: 0;
			left: 0;
			height: 100dvh;
			width: 248px;
			padding-top: 24px;
			background: var(--bg-raised);
			box-shadow: var(--shadow);
			transition: transform 380ms var(--ease-out);
		}
		.sidebar.collapsed {
			width: 248px;
			transform: translateX(-100%);
			box-shadow: none;
		}
		.collapsed .label,
		.collapsed .section span {
			opacity: 1;
			transform: none;
		}
		.collapsed .section::after {
			opacity: 0;
		}
	}
</style>
