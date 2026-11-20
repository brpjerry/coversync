<script lang="ts">
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import LibraryBig from '@lucide/svelte/icons/library-big';
	import { coverUrl } from '#lib/api.ts';
	import type { BatchItem, ImportBatch } from '#lib/batch.svelte.ts';
	import { count } from '#lib/format.ts';

	interface Props {
		batch: ImportBatch;
		/** Back to a failed album's dialog, in this batch. */
		onretry: (item: BatchItem) => void;
		/** Open an imported album in the Library. */
		onshow: (item: BatchItem) => void;
		onlibrary: () => void;
		ondone: () => void;
	}
	let { batch, onretry, onshow, onlibrary, ondone }: Props = $props();

	let totals = $derived(
		[
			['imported', 'imported'],
			['importing', 'importing'],
			['skipped', 'skipped'],
			['failed', 'failed'],
			['waiting', 'not imported']
		]
			.map(([status, word]) => [batch.count(status as BatchItem['status']), word] as const)
			.filter(([n]) => n)
			.map(([n, word]) => `${count(n)} ${word}`)
			.join(' · ')
	);

	// Keys go to the summary, not the page under it.
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
		aria-labelledby="batch-title"
		transition:fly={{ y: 24, duration: 360, easing: cubicOut }}
	>
		<header>
			<h2 id="batch-title">
				{#if batch.count('importing')}
					Importing…
				{:else}
					Imported {count(batch.count('imported'))} of {count(batch.items.length)}
					{batch.items.length === 1 ? 'album' : 'albums'}
				{/if}
			</h2>
			<p class="tabular">{totals}</p>
		</header>
		<ol class="list">
			{#each batch.items as item (item.album.id)}
				<li class={item.status}>
					<span class="art">
						{#if item.album.has_cover}<img
								src={coverUrl(item.album, 256)}
								alt=""
								onerror={(e) => ((e.currentTarget as HTMLImageElement).hidden = true)}
							/>{:else}<Disc3 size={18} strokeWidth={1.4} />{/if}
					</span>
					<span class="what">
						<b>{item.album.title}</b>
						<span class="how">
							{#if item.status === 'imported'}
								<Check size={13} /> {item.dest}
							{:else if item.status === 'importing'}
								<span class="spin"><LoaderCircle size={13} /></span> Importing…
							{:else if item.status === 'failed'}
								<TriangleAlert size={13} /> {item.error}
							{:else if item.status === 'skipped'}
								Skipped: it stays in the Inbox
							{:else}
								Not imported
							{/if}
						</span>
					</span>
					{#if item.status === 'imported'}
						<button class="btn" onclick={() => onshow(item)}>Show</button>
					{:else if item.status === 'failed'}
						<button class="btn" onclick={() => onretry(item)}>Retry</button>
					{/if}
				</li>
			{/each}
		</ol>
		<footer>
			<button class="btn" onclick={onlibrary}><LibraryBig size={15} /> Show in Library</button>
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
		width: min(720px, 100%);
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
		font-size: 22px;
		letter-spacing: -0.02em;
	}
	header p {
		margin: 4px 0 0;
		color: var(--text-2);
	}
	.list {
		margin: 0;
		padding: 4px 16px 12px;
		list-style: none;
		overflow-y: auto;
	}
	li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto;
		align-items: center;
		gap: 12px;
		padding: 8px;
		border-radius: 12px;
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
		color: var(--text-2);
		overflow-wrap: anywhere;
	}
	.how :global(svg) {
		flex: none;
		margin-top: 2px;
	}
	.imported .how {
		color: var(--success);
	}
	.failed .how {
		color: var(--danger);
	}
	.skipped,
	.waiting {
		opacity: 0.7;
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
