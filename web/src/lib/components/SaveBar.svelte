<script lang="ts">
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';

	interface Props {
		changes: number;
		saving: boolean;
		saveLabel: string;
		onrevert: () => void;
		onsave: () => void;
	}
	let { changes, saving, saveLabel, onrevert, onsave }: Props = $props();
</script>

<!-- Only there while something is unsaved; a toast confirms the save. -->
{#if changes > 0 || saving}
	<div class="savebar" transition:fly={{ y: 24, duration: 320, easing: cubicOut }}>
		<span class="summary tabular">
			{#key changes}<b in:fly={{ y: 6, duration: 200 }}>{changes}</b>{/key}
			unsaved {changes === 1 ? 'change' : 'changes'}
		</span>
		<div class="buttons">
			<button class="btn" onclick={onrevert} disabled={changes === 0}>
				<Undo2 size={15} /> Revert
			</button>
			<button class="btn primary" onclick={onsave} disabled={changes === 0 || saving}>
				{#if saving}<span class="spin"><LoaderCircle size={15} /></span>{:else}<Check
						size={15}
					/>{/if}
				{saveLabel}
			</button>
		</div>
	</div>
	<!-- Phones: the bar docks to the window's bottom edge; this keeps the form's end clear of it. -->
	<div class="dock-space" aria-hidden="true"></div>
{/if}

<style>
	.savebar {
		position: sticky;
		/* Just above the player while it's showing (--dock), else near the bottom. */
		bottom: calc(var(--dock, 0px) + 24px);
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		padding: 10px 10px 10px 18px;
		border-radius: 999px;
		background: color-mix(in oklab, var(--bg-raised) 85%, transparent);
		backdrop-filter: blur(18px);
		border: 1px solid color-mix(in oklab, var(--accent) 45%, transparent);
		box-shadow:
			var(--shadow),
			0 0 0 4px color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.summary {
		font-size: 13.5px;
		color: var(--text-2);
	}
	.summary b {
		display: inline-block;
		color: var(--accent);
	}
	.buttons {
		display: flex;
		gap: 8px;
	}
	.spin {
		display: inline-grid;
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	.dock-space {
		display: none;
	}
	/* Phones: a sheet along the bottom edge, above the player, rather than a pill over the form. */
	@media (max-width: 600px) {
		.savebar {
			position: fixed;
			left: 0;
			right: 0;
			bottom: var(--dock, 0px);
			z-index: 24;
			padding: 10px 16px calc(10px + env(safe-area-inset-bottom));
			border-radius: 0;
			border-width: 1px 0 0;
			box-shadow: 0 -12px 32px -16px rgb(0 0 0 / 0.5);
		}
		.dock-space {
			display: block;
			height: 72px;
		}
	}
</style>
