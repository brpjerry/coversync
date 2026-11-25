<script lang="ts">
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import SquareCheck from '@lucide/svelte/icons/square-check';
	import { coverUrl } from '#lib/api.ts';
	import { count } from '#lib/format.ts';
	import type { SaveItem } from '#lib/tag-batch.svelte.ts';

	interface Props {
		/** The albums whose saves failed, or went wrong (files not moved). */
		items: SaveItem[];
		/** Select these albums (those still in the Library), to try again. */
		onselect?: () => void;
		ondone: () => void;
	}
	let { items, onselect, ondone }: Props = $props();
	const uid = $props.id();

	let done = $state<HTMLButtonElement>();
	$effect(() => done?.focus({ preventScroll: true }));

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			ondone();
		}
	}
</script>

<div
	class="scrim"
	role="presentation"
	onclick={(e) => e.target === e.currentTarget && ondone()}
	{onkeydown}
	transition:fade={{ duration: 200 }}
>
	<div
		class="dialog"
		role="dialog"
		aria-modal="true"
		aria-labelledby="{uid}-title"
		transition:fly={{ y: 24, duration: 360, easing: cubicOut }}
	>
		<header>
			<h2 id="{uid}-title">
				{count(items.length)}
				{items.length === 1 ? "album wasn't" : "albums weren't"} saved as asked
			</h2>
		</header>
		<ol class="list">
			{#each items as item (item.album.id)}
				<li>
					<span class="art">
						{#if item.album.has_cover}<img
								src={coverUrl(item.album, 256)}
								alt=""
								onerror={(e) => ((e.currentTarget as HTMLImageElement).hidden = true)}
							/>{:else}<Disc3 size={18} strokeWidth={1.4} />{/if}
					</span>
					<span class="what">
						<b>{item.album.title}</b>
						<span class="how"><TriangleAlert size={13} /> {item.problem}</span>
					</span>
				</li>
			{/each}
		</ol>
		<footer>
			{#if onselect}
				<button class="btn" onclick={onselect}><SquareCheck size={15} /> Select these</button>
			{/if}
			<button class="btn primary" bind:this={done} onclick={ondone}>Done</button>
		</footer>
	</div>
</div>

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 45;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		place-items: center;
		padding: 24px;
		background: rgb(0 0 0 / 0.5);
		backdrop-filter: blur(10px);
	}
	.dialog {
		width: min(640px, 100%);
		max-height: calc(100dvh - 48px);
		display: grid;
		grid-template-rows: auto minmax(0, 1fr) auto;
		border-radius: 24px;
		background: var(--bg-raised);
		border: 1px solid var(--border-strong);
		box-shadow: 0 40px 120px -30px rgb(0 0 0 / 0.75);
		overflow: hidden;
	}
	header {
		padding: 22px 24px 12px;
	}
	h2 {
		margin: 0;
		font-size: 20px;
		letter-spacing: -0.02em;
	}
	.list {
		margin: 0;
		padding: 4px 16px 12px;
		list-style: none;
		overflow-y: auto;
	}
	li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		align-items: center;
		gap: 12px;
		padding: 8px;
	}
	li + li {
		border-top: 1px solid var(--border);
	}
	.art {
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 8px;
		overflow: hidden;
		color: var(--text-3);
		background: var(--surface);
	}
	.art img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.what {
		display: grid;
		min-width: 0;
	}
	.what b {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.how {
		display: flex;
		align-items: flex-start;
		gap: 5px;
		font-size: 12.5px;
		color: var(--danger);
		overflow-wrap: anywhere;
	}
	.how :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	footer {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		padding: 14px 20px;
		border-top: 1px solid var(--border);
	}
	@media (max-width: 760px) {
		.scrim {
			padding: 0;
		}
		.dialog {
			height: 100dvh;
			max-height: 100dvh;
			border-radius: 0;
		}
	}
</style>
