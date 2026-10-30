<script lang="ts">
	import { untrack } from 'svelte';
	import { flip } from 'svelte/animate';
	import { fade } from 'svelte/transition';
	import type { Sort } from '#lib/browse.ts';
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
		/** Sorting doesn't apply here (setup screens). */
		disabled: boolean;
		/** The tabs are a live preview of the edits on the Appearance screen. */
		previewing: boolean;
	}
	let { tabs, active, label, onsort, counts = {}, disabled, previewing }: Props = $props();

	let track = $state<HTMLElement>();
	/** The row's width: tabs coming, going or changing size move the highlight. */
	let width = $state(0);
	/** The width the row has to show in. */
	let room = $state(0);
	/** Where the highlight behind the active tab sits. */
	let pill = $state<{ x: number; w: number } | null>(null);
	/** Set once the highlight is in place, so it doesn't slide in on first show. */
	let placed = $state(false);

	const activeTab = () =>
		track?.querySelector<HTMLElement>(`[data-sort="${CSS.escape(active)}"]`) ?? null;

	$effect(() => {
		void width;
		void tabs.join();
		const el = activeTab();
		pill = el && { x: el.offsetLeft, w: el.offsetWidth };
		if (el && !untrack(() => placed)) requestAnimationFrame(() => (placed = true));
	});

	// When the tabs outgrow the window, keep the active one in view.
	$effect(() => {
		activeTab()?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
	});
</script>

<div class="sort" class:idle={disabled && !previewing} inert={disabled}>
	<span class="caption"
		>Sort by{#if previewing}<em transition:fade>{' · preview'}</em>{/if}</span
	>
	<div class="scroller" class:overflowing={width > room} bind:clientWidth={room}>
		<div
			class="track"
			role="radiogroup"
			aria-label="Sort albums by"
			bind:this={track}
			bind:clientWidth={width}
		>
			{#if pill}
				<span
					class="pill"
					class:placed
					aria-hidden="true"
					style:--x="{pill.x}px"
					style:--w="{pill.w}px"
				></span>
			{/if}
			{#each tabs as id (id)}
				{@const Icon = sortIcon(id)}
				<button
					role="radio"
					data-sort={id}
					aria-checked={active === id}
					class:on={active === id}
					onclick={() => onsort(id)}
					animate:flip={{ duration: 300 }}
					transition:fade={{ duration: 180 }}
				>
					<Icon size={15} strokeWidth={1.9} />
					{label(id)}
					{#if counts[id]}
						<span
							class="count tabular"
							class:warn={id === 'attention'}
							transition:fade={{ duration: 160 }}>{counts[id]}</span
						>
					{/if}
				</button>
			{/each}
		</div>
	</div>
</div>

<style>
	.sort {
		display: flex;
		align-items: center;
		gap: 14px;
		min-width: 0;
		transition: opacity 240ms;
	}
	.sort.idle {
		opacity: 0.4;
	}
	.caption {
		flex: none;
		font-size: 11.5px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-3);
		white-space: nowrap;
	}
	.caption em {
		font-style: normal;
		color: var(--accent);
	}
	.scroller {
		min-width: 0;
		/* Room for focus rings, which the scroll box would clip. */
		margin: -4px;
		padding: 4px;
		overflow: auto hidden;
		scrollbar-width: none;
	}
	/* Fade the ends out where tabs scroll out of sight. */
	.scroller.overflowing {
		mask-image: linear-gradient(to right, #000 calc(100% - 32px), transparent);
	}
	.track {
		position: relative;
		display: flex;
		gap: 2px;
		width: max-content;
	}
	.pill {
		position: absolute;
		inset: 0 auto 0 0;
		width: var(--w);
		border-radius: 999px;
		background: color-mix(in oklab, var(--accent) 14%, var(--surface));
		box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--accent) 28%, transparent);
		transform: translateX(var(--x));
	}
	.pill.placed {
		transition:
			transform 420ms var(--ease-spring),
			width 420ms var(--ease-spring);
	}
	button {
		position: relative;
		display: inline-flex;
		align-items: center;
		gap: 7px;
		height: 32px;
		padding: 0 13px;
		border-radius: 999px;
		font-size: 13.5px;
		font-weight: 540;
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
	button :global(svg) {
		flex: none;
		transition:
			color 200ms,
			transform 300ms var(--ease-spring);
	}
	button.on :global(svg) {
		color: var(--accent);
		transform: scale(1.08);
	}
	.count {
		padding: 0 7px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-3);
		background: var(--surface-hover);
	}
	.count.warn {
		color: hsl(40 95% 70%);
		background: hsl(40 90% 55% / 0.16);
	}
</style>
