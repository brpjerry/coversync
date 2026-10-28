<script lang="ts" module>
	/** Where the back button beside the title leads. */
	export interface Back {
		label: string;
		run: () => void;
	}
</script>

<script lang="ts">
	import type { Snippet } from 'svelte';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';

	interface Props {
		title: string;
		description: string;
		back?: Back;
		children: Snippet;
	}
	let { title, description, back, children }: Props = $props();
</script>

<div class="setup">
	<header class="head" class:has-back={!!back}>
		{#if back}
			<button class="back" onclick={back.run} aria-label={back.label} title={back.label}>
				<ArrowLeft size={20} />
			</button>
		{/if}
		<h1>{title}</h1>
		<p>{description}</p>
	</header>
	{@render children()}
</div>

<style>
	.setup {
		display: grid;
		gap: 24px;
		max-width: 920px;
		margin: 0 auto;
	}
	.head {
		display: grid;
		grid-template-columns: auto 1fr;
		column-gap: 14px;
		align-items: center;
	}
	.head h1,
	.head p {
		grid-column: 2;
	}
	.head:not(.has-back) h1,
	.head:not(.has-back) p {
		grid-column: 1 / -1;
	}
	.back {
		grid-row: 1;
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 999px;
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text-2);
		transition:
			background 160ms,
			color 160ms,
			border-color 160ms,
			transform 160ms var(--ease-out);
	}
	.back :global(svg) {
		transition: transform 220ms var(--ease-out);
	}
	.back:hover {
		background: var(--surface-hover);
		border-color: var(--border-strong);
		color: var(--text);
	}
	.back:hover :global(svg) {
		transform: translateX(-2px);
	}
	.back:active {
		transform: scale(0.94);
	}
	.head h1 {
		margin: 0;
		font-size: 30px;
		letter-spacing: -0.03em;
		font-weight: 740;
	}
	.head p {
		margin: 6px 0 0;
		color: var(--text-2);
	}

	/* Building blocks shared by the setup screens. */
	/* Only the panels: album cards in previews are .card too. */
	.setup > :global(.card) {
		border-radius: 22px;
		background: var(--bg-raised);
		border: 1px solid var(--border);
		box-shadow: 0 1px 2px rgb(0 0 0 / 0.2);
		overflow: clip;
	}
	.setup :global(.card-head) {
		display: flex;
		gap: 14px;
		align-items: flex-start;
		padding: 22px 24px 18px;
		border-bottom: 1px solid var(--border);
	}
	.setup :global(.card-icon) {
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 12px;
		color: var(--accent);
		background: color-mix(in oklab, var(--accent) 14%, transparent);
		flex: none;
	}
	.setup :global(.card-head h2) {
		margin: 0;
		font-size: 18px;
		letter-spacing: -0.01em;
	}
	.setup :global(.card-head p) {
		margin: 2px 0 0;
		color: var(--text-2);
		font-size: 13.5px;
	}
	.setup :global(.card-body) {
		padding: 20px 24px 24px;
	}
	.setup :global(.btn.sm) {
		height: 32px;
		padding: 0 12px;
		font-size: 13px;
	}
	.setup :global(.notice) {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 14px 16px;
		border-radius: 14px;
		border: 1px solid color-mix(in oklab, var(--danger) 40%, transparent);
		color: var(--text-2);
	}
	.setup :global(.skeleton) {
		display: grid;
		gap: 14px;
		padding: 24px;
	}
	.setup :global(.skeleton span) {
		height: 22px;
		border-radius: 8px;
		background: linear-gradient(
				90deg,
				var(--surface) 0%,
				var(--surface-hover) 50%,
				var(--surface) 100%
			)
			0 0 / 200% 100%;
		animation: shimmer 1.4s linear infinite;
	}
	.setup :global(.skeleton span:first-child) {
		width: 40%;
	}
	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}

	@media (max-width: 760px) {
		.setup :global(.card-head),
		.setup :global(.card-body) {
			padding-inline: 16px;
		}
	}
</style>
