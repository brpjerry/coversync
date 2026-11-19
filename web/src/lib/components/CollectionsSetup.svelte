<script lang="ts">
	import { fly, slide } from 'svelte/transition';
	import Folders from '@lucide/svelte/icons/folders';
	import Plus from '@lucide/svelte/icons/plus';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import X from '@lucide/svelte/icons/x';
	import { setup } from '#lib/setup.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';

	let { back }: { back?: Back } = $props();
</script>

<SetupPane
	{back}
	title="Collections"
	description="Groups of albums, all of a franchise say, that the import can put an album in."
>
	{#if setup.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the library setup from coversyncd.
			<button class="btn sm" onclick={() => setup.load()}>Retry</button>
		</div>
	{:else if !setup.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="collections-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><Folders size={20} /></span>
				<div>
					<h2 id="collections-title">Collections</h2>
					<p>
						The import dialog offers these for each album, and None. The one picked goes in the
						files' Collection tag.
					</p>
				</div>
			</header>
			<div class="card-body list">
				{#each setup.collections as _, i}
					<div class="row" transition:slide={{ duration: 160 }}>
						<input
							bind:value={setup.collections[i]}
							placeholder="Name"
							spellcheck="false"
							autocomplete="off"
							aria-label={`Collection ${i + 1}`}
						/>
						<button
							class="remove"
							aria-label={`Remove collection ${i + 1}`}
							onclick={() => setup.collections.splice(i, 1)}><X size={15} /></button
						>
					</div>
				{/each}
				<div>
					<button class="btn sm" onclick={() => setup.collections.push('')}
						><Plus size={14} /> Add collection</button
					>
				</div>
				<p class="hint">
					In the folder structure, <code>{'{collection}'}</code> is the album's collection, and is
					left out for albums in none: <code>{'{collection}/{album}'}</code> is just
					<code>{'{album}'}</code> for them. Renaming or removing a collection here leaves the albums
					already in it as they are; change those in the tag editor.
				</p>
			</div>
		</section>

		<SaveBar
			changes={setup.changes}
			saving={setup.saving}
			saveLabel="Save"
			onrevert={() => setup.revert()}
			onsave={() => setup.save()}
		/>
	{/if}
</SetupPane>

<style>
	.list {
		display: grid;
		gap: 10px;
	}
	.row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: 8px;
		align-items: center;
	}
	input {
		height: 40px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text);
		font: inherit;
		outline: 0;
		transition:
			border-color 160ms,
			box-shadow 200ms;
	}
	input:focus {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.remove {
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border-radius: 8px;
		color: var(--text-3);
	}
	.remove:hover {
		color: var(--text);
		background: color-mix(in oklab, var(--text) 8%, transparent);
	}
	.hint {
		margin: 0;
		font-size: 13px;
		color: var(--text-3);
	}
	.hint code {
		font-size: 12.5px;
		color: var(--text-2);
	}
</style>
