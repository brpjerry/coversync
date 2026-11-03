<script lang="ts">
	import { fly, slide } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import Copy from '@lucide/svelte/icons/copy';
	import Move from '@lucide/svelte/icons/move';
	import Inbox from '@lucide/svelte/icons/inbox';
	import LibraryBig from '@lucide/svelte/icons/library-big';
	import EyeOff from '@lucide/svelte/icons/eye-off';
	import Eye from '@lucide/svelte/icons/eye';
	import FolderSearch from '@lucide/svelte/icons/folder-search';
	import FolderField from './FolderField.svelte';
	import Toggle from './Toggle.svelte';
	import WandSparkles from '@lucide/svelte/icons/wand-sparkles';
	import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import Sparkles from '@lucide/svelte/icons/sparkles';
	import KeyRound from '@lucide/svelte/icons/key-round';
	import AudioWaveform from '@lucide/svelte/icons/audio-waveform';
	import ExternalLink from '@lucide/svelte/icons/external-link';
	import Undo2 from '@lucide/svelte/icons/undo-2';
	import type { IngestMode } from '#lib/api.ts';
	import { count } from '#lib/format.ts';
	import { inboxSetup } from '#lib/inbox-setup.svelte.ts';
	import { toast } from '#lib/toast.svelte.ts';
	import SetupPane, { type Back } from './SetupPane.svelte';
	import SaveBar from './SaveBar.svelte';

	interface Props {
		/** The set of visible Inbox files changed (e.g. after unhiding). */
		onchanged: () => void;
		/** The Inbox folder changed, so a rescan started. */
		onrescan: () => void;
		back: Back;
	}
	let { onchanged, onrescan, back }: Props = $props();

	inboxSetup.load();

	const modes: { id: IngestMode; label: string; icon: typeof Copy; description: string }[] = [
		{
			id: 'copy',
			label: 'Copy',
			icon: Copy,
			description:
				'The Library gets its own copy. The original stays in the Inbox folder and is hidden from the Inbox.'
		},
		{
			id: 'move',
			label: 'Move',
			icon: Move,
			description: 'The file itself moves into the Library folder and leaves the Inbox folder.'
		}
	];

	/** Typing a key over a saved one; the saved one shows until then. */
	let replacing = $state(false);
	let keyInput = $state<HTMLInputElement>();
	function editKey() {
		replacing = true;
		inboxSetup.keyDraft = null;
		requestAnimationFrame(() => keyInput?.focus());
	}
	$effect(() => {
		// Saved or reverted: back to showing the saved key.
		if (inboxSetup.keyDraft === null && !inboxSetup.changes) replacing = false;
	});
	let showInput = $derived(!inboxSetup.keyHint || replacing);

	/** Move deletes originals; a read-only Inbox folder can't allow that. */
	let moveBlocked = $derived(!!inboxSetup.folderStatus && !inboxSetup.folderStatus.writable);
	let confirming = $state(false);

	async function unhide() {
		const n = await inboxSetup.unhideAll();
		confirming = false;
		if (n === null) return;
		toast(
			n
				? `${count(n)} ${n === 1 ? 'file is' : 'files are'} back in the Inbox`
				: 'Nothing was hidden',
			'success'
		);
		if (n) onchanged();
	}
</script>

<SetupPane
	{back}
	title="Inbox setup"
	description="How coversync treats the music it finds in your Inbox folders."
>
	{#if inboxSetup.failed}
		<div class="notice" role="alert">
			<TriangleAlert size={18} /> Couldn't load the Inbox setup from coversyncd.
			<button class="btn sm" onclick={() => inboxSetup.load()}>Retry</button>
		</div>
	{:else if !inboxSetup.loaded}
		<div class="card skeleton" aria-busy="true"><span></span><span></span><span></span></div>
	{:else}
		<section class="card" aria-labelledby="folder-title" in:fly={{ y: 12, duration: 420 }}>
			<header class="card-head">
				<span class="card-icon"><FolderSearch size={20} /></span>
				<div>
					<h2 id="folder-title">Inbox folder</h2>
					<p>
						coversyncd scans this folder for music it doesn't manage yet. Changing it rescans the
						Inbox.
					</p>
				</div>
			</header>
			<div class="card-body">
				<FolderField
					id="inbox-folder"
					label="Folder"
					placeholder="/data/Music"
					purpose="inbox"
					bind:value={inboxSetup.folder}
				/>
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="ingest-title"
			in:fly={{ y: 12, duration: 420, delay: 60 }}
		>
			<header class="card-head">
				<span class="card-icon"><FolderInput size={20} /></span>
				<div>
					<h2 id="ingest-title">Ingest</h2>
					<p>What happens to a file when it's brought from the Inbox into the Library.</p>
				</div>
			</header>
			<div class="card-body">
				<div class="modes" role="radiogroup" aria-labelledby="ingest-title">
					{#each modes as m (m.id)}
						<label class="mode {m.id}" class:on={inboxSetup.ingest === m.id}>
							<input type="radio" name="ingest" value={m.id} bind:group={inboxSetup.ingest} />
							<span class="diagram" aria-hidden="true">
								<span class="box from"><Inbox size={15} /></span>
								<span class="path"></span>
								<span class="box to"><LibraryBig size={15} /></span>
								<span class="ghost"></span>
								<span class="chip"></span>
							</span>
							<span class="mode-title">
								<m.icon size={16} />
								{m.label}
								{#if m.id === 'copy'}<span class="default">Default</span>{/if}
							</span>
							<span class="mode-desc">{m.description}</span>
						</label>
					{/each}
				</div>

				{#if inboxSetup.ingest === 'move' && moveBlocked}
					<div class="warn" role="note" transition:slide={{ duration: 260, easing: cubicOut }}>
						<TriangleAlert size={18} />
						<p>
							<strong>Move would fail here.</strong> coversyncd can't change files in
							{inboxSetup.folderStatus?.path}, so it couldn't take them out of the Inbox folder.
							Copy works with read-only folders.
						</p>
					</div>
				{/if}
			</div>
		</section>

		{#if inboxSetup.autofill}
			<section
				class="card"
				aria-labelledby="autofill-title"
				in:fly={{ y: 12, duration: 420, delay: 90 }}
			>
				<header class="card-head">
					<span class="card-icon"><WandSparkles size={20} /></span>
					<div>
						<h2 id="autofill-title">Autofill empty tags</h2>
						<p>
							When an album is imported, fill empty tags with values that are safe to assume. Tags a
							file already has are never changed, and anything you type in the import editor wins.
						</p>
					</div>
				</header>
				<div class="card-body toggles">
					<h3>Artists</h3>
					<Toggle
						bind:checked={inboxSetup.autofill.album_artist_from_artist}
						label="Album artist from artist"
						description="When every track has the same artist."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.various_artists}
						label="“Various Artists” when the artists differ"
						description="For compilations, instead of leaving album artist empty."
						disabled={!inboxSetup.autofill.album_artist_from_artist}
						nested
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.artist_from_album_artist}
						label="Artist from album artist"
						description="For tracks that only have an album artist."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.album_tags_from_tracks}
						label="Album tags from the other tracks"
						description="Album, album artist, year, genre, label… copied to tracks missing them, when the rest agree."
					/>

					<h3>Discs and numbering</h3>
					<Toggle
						bind:checked={inboxSetup.autofill.single_track}
						label="Track 1 of 1 for one-track albums"
						description="Singles whose only track has no number."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.single_disc}
						label="Disc 1 of 1 for single-disc albums"
						description="When no track has a disc number, there are no CD1 / Disc 2 folders, and no track number repeats."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.disc_from_folder}
						label="Disc number from CD1 / Disc 2 folders"
						description="Also sets the disc total."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.totals}
						label="Track and disc totals"
						description="Track total when a disc is numbered 1 to N; disc total from the highest disc."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.track_from_file_name}
						label="Track number from the file name"
						description="“03 Title.flac” → track 3; “2-05 Title.flac” → disc 2, track 5."
					/>

					<h3>Titles and folders</h3>
					<Toggle
						bind:checked={inboxSetup.autofill.title_from_file_name}
						label="Title from the file name"
						description="“03 Title.flac” → “Title”."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.album_from_folder}
						label="Album from the folder name"
						description="Uses the album folder's name as is; off by default since folders often include the artist."
					/>
					<Toggle
						bind:checked={inboxSetup.autofill.year_from_folder}
						label="Year from the folder name"
						description="“Thaw (2015)” → 2015."
					/>
				</div>
			</section>
		{/if}

		<section
			class="card"
			aria-labelledby="suggest-title"
			in:fly={{ y: 12, duration: 420, delay: 100 }}
		>
			<header class="card-head">
				<span class="card-icon"><Sparkles size={20} /></span>
				<div>
					<h2 id="suggest-title">Smart suggestions</h2>
					<p>
						Look each album up on MusicBrainz when importing it, by its audio where AcoustID knows
						it, and offer MusicBrainz's tags: corrections where a tag differs, and values for empty
						ones. You pick which to keep.
					</p>
				</div>
			</header>
			<div class="card-body toggles">
				<Toggle
					bind:checked={inboxSetup.suggestions}
					label="Suggest tags from MusicBrainz"
					description="The import dialog opens for every album, with the suggestions to review."
				/>
				{#if inboxSetup.suggestions}
					<div transition:slide={{ duration: 260, easing: cubicOut }}>
						<Toggle
							bind:checked={inboxSetup.preferEnglish}
							label="Prefer English"
							description="For albums in scripts that don't sort A⁠–⁠Z, like Japanese, Korean or Cyrillic, suggest MusicBrainz's romanized or English titles and artist names, ticked where they replace the original script. Tags that already sort A⁠–⁠Z are kept."
							nested
						/>
						<Toggle
							bind:checked={inboxSetup.familyNameFirst}
							label="Reverse first and last names"
							description="Romanized Japanese, Chinese and Korean names of people put the family name first, as those languages do: “Nishikino Maki”, not “Maki Nishikino”. MusicBrainz's sort names tell which name is which; groups and single names stay as they are."
							disabled={!inboxSetup.preferEnglish}
							nested
						/>
					</div>
					<div class="suggest-setup" transition:slide={{ duration: 260, easing: cubicOut }}>
						<label class="key-label" for="acoustid-key">
							<KeyRound size={15} /> AcoustID API key
						</label>
						<div class="key-row">
							{#if showInput}
								<input
									id="acoustid-key"
									class="key-input"
									bind:this={keyInput}
									value={inboxSetup.keyDraft ?? ''}
									oninput={(e) => {
										const v = e.currentTarget.value;
										inboxSetup.keyDraft = v.trim() ? v : null;
									}}
									placeholder={replacing ? 'Paste the new key' : 'Paste your application key'}
									autocomplete="off"
									spellcheck="false"
								/>
								{#if replacing}
									<button
										class="btn sm"
										onclick={() => {
											replacing = false;
											inboxSetup.keyDraft = null;
										}}>Keep the saved key</button
									>
								{/if}
							{:else if inboxSetup.keyDraft === ''}
								<span class="key-saved removing">The saved key is removed when you save.</span>
								<button class="btn sm" onclick={() => (inboxSetup.keyDraft = null)}>
									<Undo2 size={14} /> Keep it
								</button>
							{:else}
								<span class="key-saved" id="acoustid-key">
									<span class="dots" aria-hidden="true">••••••</span>{inboxSetup.keyHint}
									<span class="sr-only">Saved key ending in {inboxSetup.keyHint}</span>
								</span>
								<button class="btn sm" onclick={editKey}>Replace</button>
								<button class="btn sm" onclick={() => (inboxSetup.keyDraft = '')}>Remove</button>
							{/if}
						</div>
						<p class="key-help">
							{#if !inboxSetup.keyHint && !inboxSetup.keyDraft}
								Without a key, albums are matched by their title and artist only.
							{/if}
							<a href="https://acoustid.org/new-application" target="_blank" rel="noreferrer"
								>Get a free key from AcoustID <ExternalLink size={12} /></a
							>
						</p>
						<p class="fpcalc" class:missing={!inboxSetup.fpcalc}>
							{#if inboxSetup.fpcalc}
								<AudioWaveform size={15} /> Fingerprints with fpcalc {inboxSetup.fpcalc}.
							{:else}
								<TriangleAlert size={15} /> fpcalc isn't installed, so albums are matched by their title
								and artist. It comes with the chromaprint package.
							{/if}
						</p>
					</div>
				{/if}
			</div>
		</section>

		<section
			class="card"
			aria-labelledby="hidden-title"
			in:fly={{ y: 12, duration: 420, delay: 80 }}
		>
			<header class="card-head">
				<span class="card-icon"><EyeOff size={20} /></span>
				<div>
					<h2 id="hidden-title">Hidden files</h2>
					<p>
						Files already copied into the Library are hidden here, so the Inbox only shows what is
						left to bring in.
					</p>
				</div>
			</header>
			<div class="card-body hidden-row">
				<p class="hidden-stat tabular">
					<b>{count(inboxSetup.hidden.files)}</b>
					{inboxSetup.hidden.files === 1 ? 'file' : 'files'} hidden
					{#if inboxSetup.hidden.albums}
						<span class="sub">
							across {count(inboxSetup.hidden.albums)}
							{inboxSetup.hidden.albums === 1 ? 'album' : 'albums'}</span
						>
					{/if}
				</p>
				{#if confirming}
					<div class="confirm" in:fly={{ x: 12, duration: 220 }}>
						<span>Show them in the Inbox again?</span>
						<button class="btn sm" onclick={() => (confirming = false)}>Cancel</button>
						<button class="btn sm primary" onclick={unhide} disabled={inboxSetup.unhiding}>
							{#if inboxSetup.unhiding}<span class="spin"><LoaderCircle size={14} /></span>{/if}
							Unhide all
						</button>
					</div>
				{:else}
					<button
						class="btn"
						onclick={() => (confirming = true)}
						disabled={inboxSetup.hidden.files === 0}
						title={inboxSetup.hidden.files === 0 ? 'No files are hidden' : undefined}
					>
						<Eye size={16} /> Unhide all
					</button>
				{/if}
			</div>
		</section>

		<SaveBar
			changes={inboxSetup.changes}
			saving={inboxSetup.saving}
			saveLabel="Save"
			onrevert={() => inboxSetup.revert()}
			onsave={async () => {
				if (await inboxSetup.save()) onrescan();
			}}
		/>
	{/if}
</SetupPane>

<style>
	.modes {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 14px;
	}
	.mode {
		position: relative;
		display: grid;
		gap: 6px;
		padding: 16px 18px 18px;
		border-radius: 16px;
		border: 1px solid var(--border);
		background: var(--surface);
		cursor: pointer;
		transition:
			border-color 200ms,
			background 200ms,
			box-shadow 260ms,
			transform 200ms var(--ease-out);
	}
	.mode:hover:not(.on) {
		border-color: var(--border-strong);
		transform: translateY(-1px);
	}
	.mode.on {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: color-mix(in oklab, var(--accent) 8%, var(--surface));
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 12%, transparent);
	}
	/* The radio stays native (keyboard and screen readers) but sits in the corner. */
	.mode input {
		position: absolute;
		top: 16px;
		right: 16px;
		appearance: none;
		margin: 0;
		width: 18px;
		height: 18px;
		border-radius: 999px;
		border: 1.5px solid var(--border-strong);
		background: var(--bg);
		cursor: pointer;
		transition:
			border-color 160ms,
			box-shadow 220ms var(--ease-spring);
	}
	.mode input:checked {
		border-color: var(--accent);
		box-shadow: inset 0 0 0 4px var(--bg-raised);
		background: var(--accent);
	}
	.mode-title {
		display: flex;
		align-items: center;
		gap: 8px;
		font-weight: 620;
		font-size: 15px;
	}
	.on .mode-title :global(svg) {
		color: var(--accent);
	}
	.default {
		padding: 1px 8px;
		border-radius: 999px;
		font-size: 11px;
		font-weight: 600;
		color: var(--text-3);
		background: var(--surface-hover);
	}
	.mode-desc {
		font-size: 13.5px;
		color: var(--text-2);
	}

	/* A file travelling from Inbox to Library; Copy leaves a hidden ghost behind. */
	.diagram {
		position: relative;
		height: 52px;
		margin: 2px 26px 8px 0;
	}
	.box {
		position: absolute;
		top: 6px;
		display: grid;
		place-items: center;
		width: 40px;
		height: 40px;
		border-radius: 11px;
		border: 1px solid var(--border-strong);
		background: var(--bg-raised);
		color: var(--text-3);
		z-index: 1;
		transition:
			color 200ms,
			border-color 200ms;
	}
	.from {
		left: 0;
	}
	.to {
		right: 0;
	}
	.on .box {
		color: var(--text-2);
	}
	.path {
		position: absolute;
		left: 48px;
		right: 48px;
		top: 26px;
		border-top: 1.5px dashed var(--border-strong);
	}
	.chip,
	.ghost {
		position: absolute;
		top: 19px;
		left: 12px;
		width: 16px;
		height: 14px;
		border-radius: 4px;
		z-index: 2;
	}
	/* The file only appears on the chosen mode, so idle cards show both folders. */
	.chip {
		opacity: 0;
		background: var(--accent);
	}
	.ghost {
		border: 1.5px dashed var(--text-3);
		opacity: 0;
	}
	.on .chip {
		background: var(--accent);
		box-shadow: 0 4px 12px -2px color-mix(in oklab, var(--accent) 60%, transparent);
		animation: travel 3.2s var(--ease-out) infinite;
	}
	.copy.on .ghost {
		border-color: var(--accent);
		animation: ghost 3.2s linear infinite;
	}
	@keyframes travel {
		0%,
		12% {
			left: 12px;
			opacity: 1;
		}
		55%,
		82% {
			left: calc(100% - 28px);
			opacity: 1;
		}
		95%,
		100% {
			left: calc(100% - 28px);
			opacity: 0;
		}
	}
	@keyframes ghost {
		0%,
		20% {
			opacity: 0;
		}
		35%,
		82% {
			opacity: 0.5;
		}
		95%,
		100% {
			opacity: 0;
		}
	}

	.warn {
		display: flex;
		gap: 12px;
		align-items: flex-start;
		margin-top: 14px;
		padding: 12px 16px;
		border-radius: 12px;
		background: var(--warn-surface);
		border: 1px solid var(--warn-border);
		color: var(--warn-text);
		font-size: 13.5px;
	}
	.warn :global(svg) {
		flex: none;
		margin-top: 1px;
		color: var(--warn);
	}
	.warn p {
		margin: 0;
	}

	.toggles {
		display: grid;
		gap: 2px;
		padding-top: 12px;
	}
	.toggles h3 {
		margin: 14px 0 4px;
		padding: 0 14px;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--text-3);
	}
	.toggles h3:first-child {
		margin-top: 0;
	}
	.suggest-setup {
		display: grid;
		gap: 8px;
		/* Under the switch's label, like a nested option. */
		margin: 2px 0 6px 38px;
	}
	.key-label {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		font-size: 13px;
		font-weight: 600;
		color: var(--text-2);
	}
	.key-row {
		display: flex;
		align-items: center;
		gap: 8px;
		flex-wrap: wrap;
	}
	.key-input {
		flex: 1 1 240px;
		max-width: 360px;
		height: 36px;
		padding: 0 12px;
		border-radius: 10px;
		border: 1px solid var(--border-strong);
		background: var(--surface);
		font-family: var(--mono, ui-monospace, monospace);
		font-size: 13px;
		color: var(--text);
	}
	.key-input:focus {
		outline: none;
		border-color: var(--accent);
		box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 25%, transparent);
	}
	.key-saved {
		display: inline-flex;
		align-items: center;
		gap: 2px;
		height: 36px;
		padding: 0 12px;
		border-radius: 10px;
		background: var(--surface);
		border: 1px solid var(--border);
		font-family: var(--mono, ui-monospace, monospace);
		font-size: 13px;
	}
	.key-saved.removing {
		font-family: inherit;
		color: var(--text-2);
	}
	.dots {
		color: var(--text-3);
		letter-spacing: 0.1em;
	}
	.key-help,
	.fpcalc {
		margin: 0;
		font-size: 12.5px;
		color: var(--text-3);
	}
	.key-help a {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		color: var(--accent);
		text-decoration: none;
	}
	.key-help a:hover {
		text-decoration: underline;
	}
	.fpcalc {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.fpcalc.missing {
		color: var(--warn);
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}

	.hidden-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 16px;
		flex-wrap: wrap;
	}
	.hidden-stat {
		margin: 0;
		color: var(--text-2);
	}
	.hidden-stat b {
		font-size: 22px;
		color: var(--text);
		margin-right: 2px;
	}
	.sub {
		color: var(--text-3);
	}
	.confirm {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 13.5px;
		color: var(--text-2);
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

	@media (max-width: 600px) {
		.suggest-setup {
			margin-left: 0;
		}
	}
	@media (max-width: 760px) {
		.modes {
			grid-template-columns: 1fr;
		}
	}
</style>
