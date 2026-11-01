<script lang="ts">
	import FolderOpen from '@lucide/svelte/icons/folder-open';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import FolderPlus from '@lucide/svelte/icons/folder-plus';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import { fade } from 'svelte/transition';
	import { api, ApiError, type FolderStatus } from '#lib/api.ts';

	interface Props {
		value: string;
		id: string;
		label: string;
		placeholder: string;
		/** Inbox folders must exist; a Library folder can be created on first import. */
		purpose: 'inbox' | 'library';
	}
	let { value = $bindable(), id, label, placeholder, purpose }: Props = $props();

	let status = $state<FolderStatus | null>(null);
	let error = $state<string | null>(null);
	let checking = $state(false);
	let seq = 0;

	// Ask the daemon about the folder as it's typed, after a short pause.
	$effect(() => {
		const path = value.trim();
		const mine = ++seq;
		error = null;
		if (!path) {
			status = null;
			checking = false;
			return;
		}
		checking = true;
		const timer = setTimeout(async () => {
			try {
				const s = await api.checkFolder(path);
				if (mine === seq) status = s;
			} catch (e) {
				if (mine === seq) {
					status = null;
					error = e instanceof ApiError ? e.message : 'Could not check this folder';
				}
			} finally {
				if (mine === seq) checking = false;
			}
		}, 300);
		return () => clearTimeout(timer);
	});

	type Tone = 'ok' | 'info' | 'warn' | 'error';
	let verdict = $derived.by((): { tone: Tone; text: string } | null => {
		if (error) return { tone: 'error', text: error };
		if (!value.trim())
			return purpose === 'library'
				? { tone: 'warn', text: 'Not set. Imports need a library folder.' }
				: { tone: 'warn', text: 'Not set. Nothing is scanned.' };
		if (!status) return null;
		if (status.exists && !status.dir)
			return { tone: 'error', text: "That's a file, not a folder." };
		if (!status.exists) {
			if (purpose === 'inbox') return { tone: 'error', text: 'Folder not found.' };
			return status.creatable
				? { tone: 'info', text: "Doesn't exist yet; created on the first import." }
				: { tone: 'error', text: "Doesn't exist, and coversyncd can't create it there." };
		}
		if (!status.writable)
			return purpose === 'inbox'
				? { tone: 'warn', text: 'Found. Read-only to coversyncd, so only Copy works.' }
				: { tone: 'error', text: 'Read-only to coversyncd; imports would fail.' };
		return { tone: 'ok', text: purpose === 'inbox' ? 'Found and writable.' : 'Ready for imports.' };
	});
</script>

<div class="folder-field">
	<label for={id}>{label}</label>
	<div class="row" class:bad={verdict?.tone === 'error'}>
		<FolderOpen size={17} />
		<input
			{id}
			bind:value
			{placeholder}
			spellcheck="false"
			autocomplete="off"
			autocapitalize="off"
		/>
	</div>
	{#if checking}
		<p class="verdict muted"><span class="spin"><LoaderCircle size={14} /></span> Checking…</p>
	{:else if verdict}
		{#key verdict.text}
			<p class="verdict {verdict.tone}" in:fade={{ duration: 160 }}>
				{#if verdict.tone === 'ok'}<CircleCheck size={14} />
				{:else if verdict.tone === 'info'}<FolderPlus size={14} />
				{:else if verdict.tone === 'warn'}<CircleAlert size={14} />
				{:else}<CircleX size={14} />{/if}
				{verdict.text}
			</p>
		{/key}
	{/if}
</div>

<style>
	.folder-field {
		display: grid;
		gap: 8px;
	}
	label {
		font-size: 12.5px;
		font-weight: 550;
		color: var(--text-2);
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		height: 44px;
		padding: 0 14px;
		border-radius: 12px;
		border: 1px solid var(--border);
		background: var(--surface);
		color: var(--text-3);
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.row:focus-within {
		border-color: color-mix(in oklab, var(--accent) 70%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 18%, transparent);
	}
	.row.bad {
		border-color: color-mix(in oklab, var(--danger) 60%, transparent);
	}
	input {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: 0;
		background: none;
		color: var(--text);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 14px;
	}
	.verdict {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
		font-size: 13px;
	}
	.verdict :global(svg) {
		flex: none;
	}
	.ok {
		color: var(--success);
	}
	.info {
		color: var(--accent);
	}
	.warn {
		color: var(--warn);
	}
	.error {
		color: var(--danger);
	}
	.muted {
		color: var(--text-3);
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
</style>
