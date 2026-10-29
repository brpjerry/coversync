<script lang="ts" module>
	import type { Component } from 'svelte';

	export interface MenuItem {
		label: string;
		icon?: Component<{ size?: number }>;
		run: () => void;
		/** Emphasized action, e.g. Import. */
		primary?: boolean;
		/** Where the user is now, e.g. the open setup screen. */
		current?: boolean;
		/** A word after the label, e.g. "Unsaved". */
		note?: string;
	}
	export type MenuEntry = MenuItem | 'separator';
</script>

<script lang="ts">
	import { onMount } from 'svelte';
	import { fade, scale } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';

	interface Props {
		x: number;
		y: number;
		title?: string;
		items: MenuEntry[];
		/** The button that opened the menu: clicking it again isn't clicking away, and keyboard dismissal returns to it. */
		anchor?: HTMLElement;
		onclose: () => void;
	}
	let { x, y, title, items, anchor, onclose }: Props = $props();

	let menu = $state<HTMLElement>();
	// Placed in onMount, once the menu's size is known.
	let left = $state(0);
	let top = $state(0);
	let origin = $state('top left');

	const buttons = () => [...(menu?.querySelectorAll<HTMLButtonElement>('[role=menuitem]') ?? [])];

	onMount(() => {
		// Keep the menu on screen, opening up/left from the pointer near the edges.
		const r = menu!.getBoundingClientRect();
		const flipX = x + r.width > innerWidth - 8;
		const flipY = y + r.height > innerHeight - 8;
		left = Math.max(8, flipX ? x - r.width : x);
		top = Math.max(8, flipY ? y - r.height : y);
		origin = `${flipY ? 'bottom' : 'top'} ${flipX ? 'right' : 'left'}`;
		buttons()[0]?.focus({ preventScroll: true });

		const away = (e: Event) => {
			const target = e.target as Node;
			if (!menu?.contains(target) && !anchor?.contains(target)) onclose();
		};
		const close = () => onclose();
		// Defer so the opening right-click doesn't count as clicking away.
		const timer = setTimeout(() => {
			addEventListener('pointerdown', away, true);
			addEventListener('contextmenu', away, true);
		});
		addEventListener('resize', close);
		addEventListener('scroll', close, true);
		addEventListener('blur', close);
		return () => {
			clearTimeout(timer);
			removeEventListener('pointerdown', away, true);
			removeEventListener('contextmenu', away, true);
			removeEventListener('resize', close);
			removeEventListener('scroll', close, true);
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
	role="menu"
	tabindex="-1"
	aria-label={title}
	bind:this={menu}
	style:left="{left}px"
	style:top="{top}px"
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
				role="menuitem"
				class:primary={item.primary}
				class:current={item.current}
				aria-current={item.current ? 'page' : undefined}
				onclick={() => {
					onclose();
					item.run();
				}}
			>
				{#if item.icon}<item.icon size={16} />{/if}
				{item.label}
				{#if item.note}<span class="note">{item.note}</span>{/if}
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
		padding: 6px;
		border-radius: 14px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow:
			0 18px 50px -12px rgb(0 0 0 / 0.6),
			0 2px 6px rgb(0 0 0 / 0.3);
		outline: none;
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
	button:hover,
	button:focus-visible {
		background: var(--surface-hover);
	}
	button.primary :global(svg),
	button.current :global(svg) {
		color: var(--accent);
	}
	button.current {
		font-weight: 600;
	}
	.note {
		margin-left: auto;
		padding-left: 12px;
		font-size: 12px;
		font-weight: 500;
		color: var(--accent);
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
