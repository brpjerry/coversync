<script lang="ts">
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Check from '@lucide/svelte/icons/check';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import { ART, coverUrl } from '#lib/api.ts';
	import type { BatchItem, ImportBatch } from '#lib/batch.svelte.ts';
	import ContextMenu from './ContextMenu.svelte';

	interface Props {
		batch: ImportBatch;
		/** Go to the album at this place in the batch. */
		onjump: (index: number) => void;
		label: (kind: string) => string;
	}
	let { batch, onjump, label }: Props = $props();

	type Mark = 'looking' | 'needs' | 'importing' | 'imported' | 'skipped' | 'failed' | null;

	/** An album's place in the batch, as a mark on its cover and a few words. */
	function standing(item: BatchItem): { mark: Mark; text: string } {
		switch (item.status) {
			case 'importing':
				return { mark: 'importing', text: 'Importing…' };
			case 'imported':
				return { mark: 'imported', text: `Imported to ${item.dest ?? 'the Library'}` };
			case 'skipped':
				return { mark: 'skipped', text: 'Skipped' };
			case 'failed':
				return { mark: 'failed', text: item.error ?? 'The import failed' };
		}
		if (item.error) return { mark: 'failed', text: item.error };
		if (item.lookup === 'running') return { mark: 'looking', text: 'Looking up…' };
		// Once its dialog has been shown, it says, as the album is edited there.
		if (item.needs !== undefined) {
			return item.needs ? { mark: 'needs', text: item.needs } : { mark: null, text: 'Ready' };
		}
		const p = item.plan;
		if (!p) return { mark: null, text: 'Waiting' };
		const needs = [
			...[...p.failing, ...p.disagree].map((k) => label(k)),
			...(p.art.missing ? [label(ART)] : [])
		];
		if (needs.length) return { mark: 'needs', text: `Needs ${needs.join(', ')}` };
		if (p.problems.length) return { mark: 'needs', text: p.problems[0] };
		return { mark: null, text: 'Ready' };
	}

	let button = $state<HTMLButtonElement>();
	let menu = $state<{ x: number; y: number } | null>(null);
	function list() {
		if (menu || !button) return (menu = null);
		const r = button.getBoundingClientRect();
		menu = { x: r.left, y: r.bottom + 6 };
	}

	// The current album's cover stays in view as the batch moves on.
	let strip = $state<HTMLElement>();
	$effect(() => {
		void batch.index;
		strip
			?.querySelector('[aria-current="true"]')
			?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
	});
</script>

<nav class="batch" aria-label="Albums in this import">
	<button
		bind:this={button}
		class="where tabular"
		onclick={list}
		aria-haspopup="menu"
		aria-expanded={!!menu}
		title="Albums in this import"
	>
		{batch.index + 1} of {batch.items.length}
		<ChevronDown size={14} />
	</button>
	<ol class="covers" bind:this={strip}>
		{#each batch.items as item, i (item.album.id)}
			{@const s = standing(item)}
			<li>
				<button
					class="cover {s.mark ?? ''}"
					aria-current={i === batch.index}
					onclick={() => onjump(i)}
					title="{item.album.title} · {s.text}"
					aria-label="{i + 1}: {item.album.title}, {s.text}"
				>
					{#if item.album.has_cover}
						<!-- Gone with a moved album: the box stays, empty. -->
						<img
							src={coverUrl(item.album, 256)}
							alt=""
							loading="lazy"
							onerror={(e) => ((e.currentTarget as HTMLImageElement).hidden = true)}
						/>
					{:else}
						<Disc3 size={16} strokeWidth={1.5} />
					{/if}
					{#if s.mark === 'imported'}
						<span class="mark"><Check size={11} strokeWidth={3.2} /></span>
					{:else if s.mark === 'importing' || s.mark === 'looking'}
						<span class="mark spin"><LoaderCircle size={11} strokeWidth={2.6} /></span>
					{:else if s.mark}
						<span class="mark"></span>
					{/if}
				</button>
			</li>
		{/each}
	</ol>
</nav>

{#if menu}
	<ContextMenu
		x={menu.x}
		y={menu.y}
		title="Albums in this import"
		items={batch.items.map((item, i) => ({
			label: `${i + 1}. ${item.album.title} · ${standing(item).text}`,
			checked: i === batch.index,
			run: () => onjump(i)
		}))}
		anchor={button}
		onclose={() => (menu = null)}
	/>
{/if}

<style>
	.batch {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
	}
	.where {
		flex: none;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		height: 28px;
		padding: 0 8px 0 10px;
		border-radius: 999px;
		font-size: 12.5px;
		font-weight: 650;
		color: var(--text-2);
		background: var(--surface);
		border: 1px solid var(--border);
	}
	.where:hover,
	.where[aria-expanded='true'] {
		color: var(--text);
		background: var(--surface-hover);
	}
	.covers {
		display: flex;
		gap: 6px;
		min-width: 0;
		margin: 0;
		/* Room for the current cover's ring, which the scroller would clip. */
		padding: 3px;
		list-style: none;
		overflow-x: auto;
		scrollbar-width: none;
	}
	.cover {
		position: relative;
		flex: none;
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border-radius: 6px;
		color: var(--text-3);
		background: var(--surface);
		transition:
			opacity 160ms,
			box-shadow 160ms;
	}
	.cover img {
		width: 100%;
		height: 100%;
		border-radius: inherit;
		object-fit: cover;
	}
	.cover[aria-current='true'] {
		box-shadow:
			0 0 0 2px var(--bg-raised),
			0 0 0 4px var(--accent);
	}
	.cover.skipped {
		opacity: 0.4;
	}
	/* What became of the album, or what it needs, in a corner of its cover. */
	.mark {
		position: absolute;
		right: -4px;
		bottom: -4px;
		display: grid;
		place-items: center;
		width: 15px;
		height: 15px;
		border-radius: 999px;
		color: #fff;
		background: var(--text-3);
		box-shadow: 0 0 0 2px var(--bg-raised);
	}
	.imported .mark {
		background: var(--success);
	}
	.needs .mark {
		width: 11px;
		height: 11px;
		right: -2px;
		bottom: -2px;
		background: var(--warn);
	}
	.failed .mark {
		width: 11px;
		height: 11px;
		right: -2px;
		bottom: -2px;
		background: var(--danger);
	}
	.importing .mark,
	.looking .mark {
		color: var(--accent-ink);
		background: var(--accent);
	}
	.spin :global(svg) {
		animation: spin 900ms linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	/* Phones: the list under "2 of 7" stands in for the covers. */
	@media (max-width: 600px) {
		.covers {
			display: none;
		}
	}
</style>
