<script lang="ts">
	import Check from '@lucide/svelte/icons/check';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import X from '@lucide/svelte/icons/x';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { flip } from 'svelte/animate';
	import { fly } from 'svelte/transition';
	import { dismiss, toasts } from '#lib/toast.svelte.ts';
</script>

<div class="toasts" aria-live="polite">
	{#each toasts as t (t.id)}
		<div
			class="toast {t.kind}"
			animate:flip={{ duration: 300 }}
			in:fly={{ y: -20, duration: 420 }}
			out:fly={{ x: 40, duration: 260 }}
		>
			{#if t.kind === 'success'}<Check size={16} />{:else if t.kind === 'error'}<TriangleAlert
					size={16}
				/>{/if}
			{#if t.kind === 'progress'}<span class="spin"><LoaderCircle size={16} /></span>{/if}
			<span>{t.message}</span>
			{#if t.action}
				<button
					class="action"
					onclick={() => {
						t.action?.run();
						dismiss(t.id);
					}}>{t.action.label}</button
				>
			{/if}
			<button class="x" onclick={() => dismiss(t.id)} aria-label="Dismiss"><X size={14} /></button>
		</div>
	{/each}
</div>

<style>
	.toasts {
		position: fixed;
		/* Top right: the bottom is taken by save bars and the player. */
		right: 20px;
		top: calc(var(--topbar-h) + 16px);
		z-index: 50;
		display: grid;
		gap: 10px;
		justify-items: end;
		pointer-events: none;
	}
	.toast {
		pointer-events: auto;
		display: flex;
		align-items: center;
		gap: 10px;
		max-width: 420px;
		padding: 10px 10px 10px 14px;
		border-radius: 14px;
		background: color-mix(in oklab, var(--bg-raised) 90%, transparent);
		backdrop-filter: blur(16px);
		border: 1px solid var(--border-strong);
		box-shadow: var(--shadow);
		font-size: 13.5px;
	}
	.success :global(svg:first-child) {
		color: var(--success);
	}
	.error {
		border-color: color-mix(in oklab, var(--danger) 45%, transparent);
	}
	.error :global(svg:first-child) {
		color: var(--danger);
	}
	.action {
		padding: 4px 10px;
		border-radius: 999px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 14%, transparent);
		white-space: nowrap;
	}
	.action:hover {
		background: color-mix(in oklab, var(--accent) 22%, transparent);
	}
	.spin {
		display: inline-grid;
		color: var(--accent);
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	.x {
		display: grid;
		place-items: center;
		width: 24px;
		height: 24px;
		border-radius: 999px;
		color: var(--text-3);
	}
	.x:hover {
		background: var(--surface-hover);
		color: var(--text);
	}
</style>
