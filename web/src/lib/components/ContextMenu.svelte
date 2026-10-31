<script lang="ts" module>
	import type { Component } from 'svelte';

	export interface MenuItem {
		label: string;
		icon?: Component<{ size?: number }>;
		run: () => void;
		/** Emphasized action, e.g. Import. */
		primary?: boolean;
		/** Set on every item of a menu that picks one of them, e.g. a sort; true on the one picked. */
		checked?: boolean;
		/** A count after the label, e.g. how many albums need attention; shown unless 0. */
		count?: number;
		/** The count is a warning. */
		warn?: boolean;
	}
	export type MenuEntry = MenuItem | 'separator';
</script>

<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { fade, scale } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import Check from '@lucide/svelte/icons/check';

	interface Props {
		x: number;
		y: number;
		title?: string;
		items: MenuEntry[];
		/** The button that opened the menu: clicking it again isn't clicking away, and picking an item or dismissing with the keyboard returns to it. */
		anchor?: HTMLElement;
		onclose: () => void;
	}
	let { x, y, title, items, anchor, onclose }: Props = $props();

	let menu = $state<HTMLElement>();
	// Placed in onMount, once the menu's size is known.
	let left = $state(0);
	let top = $state(0);
	let height = $state<number>();
	/** Taller than the room it has, so it scrolls. */
	let long = $state(false);
	let origin = $state('top left');

	const buttons = () => [...(menu?.querySelectorAll<HTMLButtonElement>('[role^=menuitem]') ?? [])];

	onMount(() => {
		// Keep the menu on screen, opening up/left from the pointer near the edges;
		// a menu too long for either side takes the roomier one and scrolls.
		const r = menu!.getBoundingClientRect();
		const below = innerHeight - 8 - y;
		const above = y - 8;
		const flipX = x + r.width > innerWidth - 8;
		const flipY = r.height > below && above > below;
		height = Math.min(r.height, flipY ? above : below);
		long = height < r.height;
		left = Math.max(8, flipX ? x - r.width : x);
		top = flipY ? y - height : y;
		origin = `${flipY ? 'bottom' : 'top'} ${flipX ? 'right' : 'left'}`;
		const list = buttons();
		const start = list.find((b) => b.getAttribute('aria-checked') === 'true') ?? list[0];
		start?.focus({ preventScroll: true });
		// Once its height applies, a long menu scrolls to centre that item.
		if (long && start)
			tick().then(() => {
				if (menu) menu.scrollTop = start.offsetTop - (menu.clientHeight - start.offsetHeight) / 2;
			});

		const away = (e: Event) => {
			const target = e.target as Node;
			if (!menu?.contains(target) && !anchor?.contains(target)) onclose();
		};
		const close = () => onclose();
		// A long menu scrolls; scrolling it isn't scrolling away.
		const scrolled = (e: Event) => {
			if (!menu?.contains(e.target as Node)) onclose();
		};
		// Defer so the opening right-click doesn't count as clicking away.
		const timer = setTimeout(() => {
			addEventListener('pointerdown', away, true);
			addEventListener('contextmenu', away, true);
		});
		addEventListener('resize', close);
		addEventListener('scroll', scrolled, true);
		addEventListener('blur', close);
		return () => {
			clearTimeout(timer);
			removeEventListener('pointerdown', away, true);
			removeEventListener('contextmenu', away, true);
			removeEventListener('resize', close);
			removeEventListener('scroll', scrolled, true);
			removeEventListener('blur', close);
		};
	});

	function onkeydown(e: KeyboardEvent) {
		const list = buttons();
		const i = list.indexOf(document.activeElement as HTMLButtonElement);
		const go = (n: number) => list[(n + list.length) % list.length]?.focus();
		if (e.key === 'ArrowDown') go(i + 1);
		else if (e.key === 'ArrowUp') go(i - 1);
		else if (e.key === 'Home') go(0);
		else if (e.key === 'End') go(list.length - 1);
		else if (e.key === 'Escape' || e.key === 'Tab') {
			onclose();
			anchor?.focus();
		} else return;
		e.preventDefault();
		e.stopPropagation();
	}
</script>

<div
	class="menu"
	class:long
	role="menu"
	tabindex="-1"
	aria-label={title}
	bind:this={menu}
	{@attach (node) => {
		// From the body, so no ancestor's stacking context (the top bar's) puts the player over it.
		document.body.append(node);
		return () => node.remove();
	}}
	style:left="{left}px"
	style:top="{top}px"
	style:max-height={height && `${height}px`}
	style:transform-origin={origin}
	{onkeydown}
	in:scale={{ start: 0.92, duration: 160, easing: cubicOut }}
	out:fade={{ duration: 100 }}
>
	{#if title}<div class="title">{title}</div>{/if}
	{#each items as item, i (i)}
		{#if item === 'separator'}
			<div class="sep" role="separator"></div>
		{:else}
			<button
				role={item.checked === undefined ? 'menuitem' : 'menuitemradio'}
				class:primary={item.primary}
				class:checked={item.checked}
				aria-checked={item.checked}
				onclick={() => {
					onclose();
					anchor?.focus({ preventScroll: true });
					item.run();
				}}
			>
				{#if item.icon}<item.icon size={16} />{/if}
				<span class="label">{item.label}</span>
				{#if item.count}<span class="count tabular" class:warn={item.warn}>{item.count}</span>{/if}
				{#if item.checked !== undefined}
					<span class="check">
						{#if item.checked}<Check size={15} strokeWidth={2.4} />{/if}
					</span>
				{/if}
			</button>
		{/if}
	{/each}
</div>

<style>
	.menu {
		position: fixed;
		z-index: 60;
		min-width: 210px;
		max-width: 300px;
		max-height: calc(100dvh - 16px);
		overflow-y: auto;
		scrollbar-width: thin;
		padding: 6px;
		border-radius: 14px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow:
			0 18px 50px -12px rgb(0 0 0 / 0.6),
			0 2px 6px rgb(0 0 0 / 0.3);
		outline: none;
	}
	/* Room for the scrollbar in the width, so it doesn't cut labels short. */
	.menu.long {
		scrollbar-gutter: stable;
	}
	.title {
		padding: 6px 10px 6px;
		font-size: 12px;
		font-weight: 600;
		color: var(--text-3);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	button {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		height: 36px;
		padding: 0 10px;
		border-radius: 9px;
		font-size: 14px;
		color: var(--text);
		text-align: left;
		outline: none;
		transition: background 120ms;
	}
	button :global(svg) {
		color: var(--text-2);
		flex: none;
	}
	.label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	button:hover,
	button:focus-visible {
		background: var(--surface-hover);
	}
	button.primary :global(svg),
	button.checked :global(svg) {
		color: var(--accent);
	}
	button.checked {
		font-weight: 600;
	}
	.count {
		margin-left: auto;
		padding: 0 7px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--text-3);
		background: var(--surface-hover);
	}
	.count.warn {
		color: color-mix(in oklab, hsl(40 90% 62%) 75%, var(--text));
		background: color-mix(in oklab, hsl(40 90% 55%) 18%, transparent);
	}
	/* Radio items keep the tick at the far end. */
	.check {
		display: grid;
		width: 15px;
		margin-left: auto;
	}
	.count + .check {
		margin-left: 0;
	}
	button.primary:hover,
	button.primary:focus-visible {
		background: color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.sep {
		height: 1px;
		margin: 5px 6px;
		background: var(--border);
	}
</style>
