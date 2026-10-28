<script lang="ts">
	import Braces from '@lucide/svelte/icons/braces';
	import CornerDownRight from '@lucide/svelte/icons/corner-down-right';
	import CircleX from '@lucide/svelte/icons/circle-x';
	import Ruler from '@lucide/svelte/icons/ruler';
	import ShowAll from './ShowAll.svelte';
	import { api, type StructurePreview } from '#lib/api.ts';
	import { fold } from '#lib/format.ts';

	interface Props {
		value: string;
		/** The library folder, shown in front of the previewed paths. */
		folder: string;
		suggestions: string[];
		variables: { id: string; label: string; group: string }[];
		label: (id: string) => string;
	}
	let { value = $bindable(), folder, suggestions, variables, label }: Props = $props();

	let input = $state<HTMLInputElement>();
	let preview = $state<StructurePreview | null>(null);
	let varFilter = $state('');
	let seq = 0;

	$effect(() => {
		const structure = value.trim();
		const mine = ++seq;
		const timer = setTimeout(async () => {
			try {
				const p = await api.preview(structure);
				if (mine === seq) preview = p;
			} catch {
				// The preview is a convenience; saving still validates.
			}
		}, 250);
		return () => clearTimeout(timer);
	});

	/** Put `{id}` at the cursor and keep editing after it. */
	function insert(id: string) {
		const el = input;
		const token = `{${id}}`;
		const start = el?.selectionStart ?? value.length;
		const end = el?.selectionEnd ?? value.length;
		value = value.slice(0, start) + token + value.slice(end);
		requestAnimationFrame(() => {
			el?.focus();
			el?.setSelectionRange(start + token.length, start + token.length);
		});
	}

	/** Core variables are one click away; the rest sit under "Show all variables". */
	let coreVars = $derived(variables.filter((v) => v.group === 'core'));
	let otherVars = $derived(variables.filter((v) => v.group !== 'core'));
	let shownVars = $derived(
		otherVars.filter((v) => fold(`${v.id} ${v.label}`).includes(fold(varFilter.trim())))
	);
	let prefix = $derived(folder.trim() ? `${folder.trim().replace(/\/$/, '')}/` : '');

	/** `{album}` or `{album:40}`: a variable and its length limit. */
	const TOKEN = /\{\s*([a-z0-9_]+)\s*(?::\s*(\d*)\s*)?\}/g;
	/** Numbers and dates are short already. */
	const UNLIMITED = new Set([
		'year',
		'date',
		'track_number',
		'track_total',
		'disc_number',
		'disc_total'
	]);
	/** Text variables the structure uses, each with its limit ('' for none). */
	let limits = $derived.by(() => {
		const out = new Map<string, string>();
		for (const [, id, max] of value.matchAll(TOKEN)) {
			if (!UNLIMITED.has(id) && !out.has(id)) out.set(id, max ?? '');
		}
		return [...out];
	});
	/** Rewrite every `{id}` token with the new limit; empty or 0 removes it. */
	function setLimit(id: string, raw: string) {
		const n = parseInt(raw, 10);
		value = value.replace(TOKEN, (token, name: string) =>
			name !== id ? token : n > 0 ? `{${id}:${n}}` : `{${id}}`
		);
	}
</script>

<div class="structure">
	<div class="row" class:bad={!!preview?.error}>
		<Braces size={17} />
		<input
			bind:this={input}
			bind:value
			spellcheck="false"
			autocomplete="off"
			aria-label="Folder structure"
		/>
	</div>

	<div class="suggestions" role="group" aria-label="Suggested structures">
		{#each suggestions as s (s)}
			<button class="suggestion" class:on={value.trim() === s} onclick={() => (value = s)}
				>{s}</button
			>
		{/each}
	</div>

	{#if limits.length}
		<div class="limits" role="group" aria-label="Maximum lengths">
			<span class="limits-label"><Ruler size={14} /> Max length</span>
			{#each limits as [id, max] (id)}
				<label class="limit" class:on={!!max}>
					<code>{id}</code>
					<input
						type="number"
						min="1"
						max="180"
						inputmode="numeric"
						placeholder="any"
						value={max}
						oninput={(e) => setLimit(id, e.currentTarget.value)}
						aria-label="Maximum length of {label(id)}, in characters"
					/>
				</label>
			{/each}
			<span class="limits-hint"
				>Characters. Longer values are cut, at a word break when one is close.</span
			>
		</div>
	{/if}

	<div class="preview" aria-live="polite">
		{#if preview?.error}
			<p class="error"><CircleX size={14} /> {preview.error}</p>
		{:else if preview}
			{#each preview.examples as e (e.from)}
				<div class="example">
					<span class="from">{e.from}</span>
					<span class="to">
						<CornerDownRight size={14} />
						{#if e.to}
							<span class="prefix">{prefix}</span><span class="path">{e.to}</span>
						{:else}
							<span class="missing">missing {e.missing.map(label).join(', ')}</span>
						{/if}
					</span>
				</div>
			{:else}
				<p class="muted">Nothing in the Inbox to preview with yet.</p>
			{/each}
			{#if preview.requires.length}
				<p class="requires">
					Imported files need
					{#each preview.requires as id, i (id)}<b>{label(id)}</b>{i < preview.requires.length - 1
							? ', '
							: ''}{/each}; these count as required tags.
				</p>
			{/if}
		{/if}
	</div>

	<div class="core-vars" role="group" aria-label="Insert a core variable">
		{#each coreVars as v (v.id)}
			<button
				class="var-chip"
				onclick={() => insert(v.id)}
				title="Insert {`{${v.id}}`} ({v.label})"
			>
				{`{${v.id}}`}
			</button>
		{/each}
	</div>
	<ShowAll label="Show all variables" count={otherVars.length}>
		<input class="var-filter" placeholder="Filter variables" bind:value={varFilter} />
		<div class="var-grid">
			{#each shownVars as v (v.id)}
				<button class="var" onclick={() => insert(v.id)} title="Insert {`{${v.id}}`}">
					<code>{`{${v.id}}`}</code><span>{v.label}</span>
				</button>
			{/each}
		</div>
	</ShowAll>
</div>

<style>
	.structure {
		display: grid;
		gap: 12px;
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
	.row input,
	code,
	.from,
	.path,
	.prefix {
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
	}
	.row input {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: 0;
		background: none;
		color: var(--text);
		font-size: 14px;
	}

	.suggestions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.suggestion {
		padding: 5px 10px;
		border-radius: 8px;
		border: 1px solid var(--border);
		background: var(--surface);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 12px;
		color: var(--text-2);
		transition:
			border-color 160ms,
			color 160ms,
			background 160ms;
	}
	.suggestion:hover {
		border-color: var(--border-strong);
		color: var(--text);
	}
	.suggestion.on {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 10%, var(--surface));
		color: var(--text);
	}

	.limits {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px 8px;
	}
	.limits-label {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		margin-right: 2px;
		font-size: 12.5px;
		font-weight: 600;
		color: var(--text-2);
	}
	.limit {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 30px;
		padding: 0 4px 0 9px;
		border-radius: 8px;
		border: 1px solid var(--border);
		background: var(--surface);
		font-size: 12px;
		color: var(--text-2);
		transition:
			border-color 160ms,
			background 160ms;
	}
	.limit.on {
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 10%, var(--surface));
		color: var(--text);
	}
	.limit:focus-within {
		border-color: var(--accent);
	}
	.limit input {
		width: 48px;
		height: 22px;
		padding: 0 6px;
		border: 0;
		border-radius: 6px;
		background: var(--bg-raised);
		color: var(--text);
		font: inherit;
		font-variant-numeric: tabular-nums;
		text-align: right;
		appearance: textfield;
		-moz-appearance: textfield;
	}
	.limit input::-webkit-inner-spin-button,
	.limit input::-webkit-outer-spin-button {
		appearance: none;
		margin: 0;
	}
	.limit input:focus {
		outline: none;
	}
	.limit input::placeholder {
		color: var(--text-3);
	}
	.limits-hint {
		flex-basis: 100%;
		font-size: 12px;
		color: var(--text-3);
	}
	.preview {
		display: grid;
		gap: 8px;
		padding: 14px 16px;
		border-radius: 14px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-size: 13px;
	}
	.example {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.from {
		color: var(--text-3);
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.to {
		display: flex;
		align-items: baseline;
		gap: 6px;
		min-width: 0;
		color: var(--text-3);
	}
	.to :global(svg) {
		flex: none;
		align-self: center;
	}
	.prefix {
		color: var(--text-3);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 40%;
		direction: rtl;
	}
	.path {
		/* Wraps rather than hiding the file name, which length limits shape. */
		flex: 1;
		min-width: 0;
		color: var(--text);
		overflow-wrap: anywhere;
	}
	.missing {
		color: var(--danger);
	}
	.requires,
	.muted {
		margin: 4px 0 0;
		color: var(--text-3);
	}
	.requires b {
		color: var(--text-2);
		font-weight: 560;
	}
	.error {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: 0;
		color: var(--danger);
	}

	.core-vars {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.var-chip {
		padding: 4px 9px;
		border-radius: 8px;
		border: 1px dashed var(--border-strong);
		font-family: ui-monospace, 'SF Mono', 'JetBrains Mono', monospace;
		font-size: 12px;
		color: var(--accent);
		transition:
			background 140ms,
			border-color 140ms,
			transform 140ms var(--ease-spring);
	}
	.var-chip:hover {
		border-style: solid;
		border-color: color-mix(in oklab, var(--accent) 55%, transparent);
		background: color-mix(in oklab, var(--accent) 10%, transparent);
	}
	.var-chip:active {
		transform: scale(0.95);
	}
	.var-filter {
		height: 36px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--border);
		background: var(--surface);
		outline: none;
	}
	.var-filter:focus {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
	}
	.var-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
		gap: 4px;
		max-height: 260px;
		overflow-y: auto;
	}
	.var {
		display: flex;
		align-items: baseline;
		gap: 8px;
		padding: 6px 10px;
		border-radius: 8px;
		text-align: left;
		min-width: 0;
	}
	.var:hover {
		background: var(--surface-hover);
	}
	.var code {
		font-size: 12px;
		color: var(--accent);
		white-space: nowrap;
	}
	.var span {
		font-size: 12.5px;
		color: var(--text-3);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
