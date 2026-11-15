<script lang="ts">
	import Upload from '@lucide/svelte/icons/upload';
	import FolderUp from '@lucide/svelte/icons/folder-up';
	import FileUp from '@lucide/svelte/icons/file-up';
	import ContextMenu from './ContextMenu.svelte';

	interface Props {
		/** Files picked: from a folder (with `webkitRelativePath`), or loose. */
		onpick: (files: File[]) => void;
	}
	let { onpick }: Props = $props();

	let button = $state<HTMLButtonElement>();
	let folders = $state<HTMLInputElement>();
	let files = $state<HTMLInputElement>();
	let menu = $state<{ x: number; y: number } | null>(null);

	function toggle() {
		if (menu || !button) return (menu = null);
		// Below the button, its right edge lined up with the button's.
		const r = button.getBoundingClientRect();
		menu = { x: r.right, y: r.bottom + 8 };
	}

	function picked(input: HTMLInputElement) {
		const chosen = [...(input.files ?? [])];
		// Picking the same again fires another change.
		input.value = '';
		if (chosen.length) onpick(chosen);
	}
</script>

<button
	bind:this={button}
	class="icon-btn"
	onclick={toggle}
	aria-label="Upload to the Inbox"
	title="Upload to the Inbox"
	aria-haspopup="menu"
	aria-expanded={!!menu}
>
	<Upload size={17} />
</button>

<!-- A folder picker uploads the folder, with what's inside it; only browsers offer it as webkitdirectory. -->
<input bind:this={folders} type="file" webkitdirectory hidden onchange={() => picked(folders!)} />
<input bind:this={files} type="file" multiple hidden onchange={() => picked(files!)} />

{#if menu}
	<ContextMenu
		x={menu.x}
		y={menu.y}
		title="Upload to the Inbox"
		items={[
			{ label: 'A folder…', icon: FolderUp, run: () => folders?.click() },
			{ label: 'Files…', icon: FileUp, run: () => files?.click() }
		]}
		anchor={button}
		onclose={() => (menu = null)}
	/>
{/if}
