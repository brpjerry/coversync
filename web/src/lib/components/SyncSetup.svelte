<script lang="ts">
	import { fly } from 'svelte/transition';
	import FolderSync from '@lucide/svelte/icons/folder-sync';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';
	import Toggle from './Toggle.svelte';

	interface Props {
		back: Back;
	}
	let { back }: Props = $props();

	syncSetup.load();
</script>

<SetupPane {back} title="Sync setup" description="Copying the Library to your devices.">
	{#if syncSetup.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the sync setup from coversyncd.
			<button class="btn sm" onclick={() => syncSetup.load()}>Retry</button>
		</div>
	{:else if !syncSetup.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="sync-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><FolderSync size={20} /></span>
				<div>
					<h2 id="sync-title">Sync</h2>
					<p>
						Sync will keep devices such as phones and music players up to date with the Library,
						each with its own settings. It isn't ready yet: for now the sidebar's Sync section,
						where those devices will be listed, is a placeholder.
					</p>
				</div>
			</header>
			<div class="card-body toggles">
				<Toggle
					bind:checked={syncSetup.enabled}
					label="Sync"
					description="When it's off, the sidebar's Sync section is hidden."
				/>
			</div>
		</section>

		<SaveBar
			changes={syncSetup.changes}
			saving={syncSetup.saving}
			saveLabel="Save"
			onrevert={() => syncSetup.revert()}
			onsave={() => syncSetup.save()}
		/>
	{/if}
</SetupPane>

<style>
	.toggles {
		padding-top: 12px;
	}
</style>
