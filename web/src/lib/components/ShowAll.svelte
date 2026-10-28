<script lang="ts">
	import type { Snippet } from 'svelte';
	import { slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';

	interface Props {
		/** e.g. "Show all tags". */
		label: string;
		/** How many items are tucked away. */
		count?: number;
		/** Hidden items that are selected, e.g. "2 required"; shown while closed. */
		badge?: string;
		/** Keep it open regardless, e.g. while a filter is typed. */
		forceOpen?: boolean;
		children: Snippet;
	}
	let { label, count, badge, forceOpen = false, children }: Props = $props();

	let open = $state(false);
	let shown = $derived(open || forceOpen);
</script>

<div class="show-all">
	<button class="toggle" aria-expanded={shown} onclick={() => (open = !shown)} disabled={forceOpen}>
		<span class="chev" class:open={shown}><ChevronRight size={15} /></span>
		{shown ? label.replace(/^Show all/, 'Hide') : label}
		{#if count !== undefined && !shown}<span class="count tabular">{count}</span>{/if}
		{#if badge && !shown}<span class="badge">{badge}</span>{/if}
	</button>
	{#if shown}
		<div class="content" transition:slide={{ duration: 280, easing: cubicOut }}>
			{@render children()}
		</div>
	{/if}
</div>

<style>
	.show-all {
		display: grid;
		gap: 12px;
	}
	.toggle {
		justify-self: start;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 4px 8px 4px 2px;
		border-radius: 8px;
		font-size: 13px;
		font-weight: 550;
		color: var(--text-2);
		transition: color 160ms;
	}
	.toggle:hover:not(:disabled) {
		color: var(--text);
	}
	.toggle:disabled {
		cursor: default;
	}
	.chev {
		display: grid;
		transition: transform 260ms var(--ease-out);
	}
	.chev.open {
		transform: rotate(90deg);
	}
	.count {
		padding: 0 7px;
		border-radius: 999px;
		font-size: 11.5px;
		color: var(--text-3);
		background: var(--surface-hover);
	}
	.badge {
		padding: 0 8px;
		border-radius: 999px;
		font-size: 11.5px;
		font-weight: 600;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.content {
		display: grid;
		gap: 12px;
	}
</style>
