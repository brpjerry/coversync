<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import { cubicOut } from 'svelte/easing';
	import { fade, fly, slide } from 'svelte/transition';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import Disc3 from '@lucide/svelte/icons/disc-3';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import CircleCheck from '@lucide/svelte/icons/circle-check';
	import FolderCog from '@lucide/svelte/icons/folder-cog';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import Inbox from '@lucide/svelte/icons/inbox';
	import LibraryBig from '@lucide/svelte/icons/library-big';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import FolderInput from '@lucide/svelte/icons/folder-input';
	import PanelTopOpen from '@lucide/svelte/icons/panel-top-open';
	import Play from '@lucide/svelte/icons/play';
	import FilterX from '@lucide/svelte/icons/funnel-x';
	import {
		api,
		ApiError,
		type Album,
		type AlbumDetail as Detail,
		type ImportEdits,
		type ImportPlan,
		type Status,
		type TagWriteResult
	} from '#lib/api.ts';
	import {
		collection,
		isSetup,
		migrateSort,
		parseView,
		readPref,
		writePref,
		VIEWS,
		type Sort,
		type View
	} from '#lib/browse.ts';
	import { setup } from '#lib/setup.svelte.ts';
	import { appearance } from '#lib/appearance.svelte.ts';
	import { inboxSetup } from '#lib/inbox-setup.svelte.ts';
	import { syncSetup } from '#lib/sync-setup.svelte.ts';
	import { devices } from '#lib/devices.svelte.ts';
	import { matcher, picked, type Filters } from '#lib/filters.ts';
	import { listen, type Listening, type NoticeHandlers, type Notices } from '#lib/events.ts';
	import { count, fold } from '#lib/format.ts';
	import { GRID_MAX_WIDTH, gridLayout } from '#lib/grid.ts';
	import { player } from '#lib/player.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
	import { desktopApp } from '#lib/desktop.svelte.ts';
	import { notify } from '#lib/notify.ts';
	import { dismiss, toast, type ToastAction } from '#lib/toast.svelte.ts';
	import TopBar from '#lib/components/TopBar.svelte';
	import SortMenu from '#lib/components/SortMenu.svelte';
	import Sidebar from '#lib/components/Sidebar.svelte';
	import LibrarySetup from '#lib/components/LibrarySetup.svelte';
	import InboxSetup from '#lib/components/InboxSetup.svelte';
	import AppearanceSetup from '#lib/components/AppearanceSetup.svelte';
	import SyncSetup from '#lib/components/SyncSetup.svelte';
	import AlbumGrid from '#lib/components/AlbumGrid.svelte';
	import AlbumDetail from '#lib/components/AlbumDetail.svelte';
	import PlayerBar from '#lib/components/PlayerBar.svelte';
	import Toasts from '#lib/components/Toasts.svelte';
	import ContextMenu, { type MenuEntry } from '#lib/components/ContextMenu.svelte';
	import ImportDialog from '#lib/components/ImportDialog.svelte';
	import type { Draft, TagDraft } from '#lib/components/TagEditor.svelte';

	const SORT_KEY = 'coversync.sort';
	const SIDEBAR_KEY = 'coversync.sidebar';
	const NARROW = '(max-width: 900px)';

	/** Scanned on-disk files: the unmanaged inbox. */
	let albums = $state<Album[]>([]);
	/** Albums coversync manages, in the Library folder. */
	let library = $state<Album[]>([]);
	let status = $state<Status | null>(null);
	let loaded = $state(false);
	let query = $state('');
	let sort = $state<Sort>(
		migrateSort(readPref(SORT_KEY, (v): v is string => typeof v === 'string', 'album_artist'))
	);
	/** The sort in use: the chosen one, or the menu's first if it was removed in Appearance. */
	let activeSort = $derived(
		appearance.sorts.saved.includes(sort) ? sort : appearance.sorts.saved[0]
	);
	let search = $state<HTMLInputElement>();
	let detail = $state<Detail | null>(null);

	let view = $derived(parseView(page.url.searchParams.get('view')));

	// On narrow screens the sidebar is a drawer that starts closed; whether
	// it's open on wider ones is remembered.
	let narrow = $state(matchMedia(NARROW).matches);
	let sidebarClosed = $state(
		readPref(SIDEBAR_KEY, (v): v is string => v === 'collapsed' || v === 'expanded', 'expanded') ===
			'collapsed'
	);
	let drawerOpen = $state(false);
	let collapsed = $derived(narrow ? !drawerOpen : sidebarClosed);

	function toggleSidebar() {
		if (narrow) drawerOpen = !drawerOpen;
		else {
			sidebarClosed = !sidebarClosed;
			writePref(SIDEBAR_KEY, sidebarClosed ? 'collapsed' : 'expanded');
		}
	}

	$effect(() => writePref(SORT_KEY, sort));

	/** The album grid last shown, or for a setup screen opened directly, its own collection. */
	let lastGrid = $state<'inbox' | 'library'>(
		collection(parseView(page.url.searchParams.get('view'))) ?? 'inbox'
	);
	$effect(() => {
		if (view === 'inbox' || view === 'library') lastGrid = view;
	});
	/**
	 * The album grid shown, or the one behind a setup screen: where its back
	 * button returns to, and what the sidebar's filters list.
	 */
	let browsing = $derived(view === 'inbox' || view === 'library' ? view : lastGrid);
	let setupBack = $derived({
		label: browsing === 'library' ? 'Back to Library' : 'Back to Inbox',
		run: () => setView(browsing)
	});
	/** Views slide in from the side of the tab they come after. */
	let direction = $state(1);
	async function setView(next: View) {
		drawerOpen = false;
		if (next === view) return;
		direction = VIEWS.indexOf(next) > VIEWS.indexOf(view) ? 1 : -1;
		query = '';
		const url = new URL(page.url.href);
		if (next === 'inbox') url.searchParams.delete('view');
		else url.searchParams.set('view', next);
		// Same page, so nothing reloads; keep focus on the tab and return to the top.
		await goto(url, { replace: true, reset: false });
		scrollTo({ top: 0 });
	}
	let source = $derived(browsing === 'library' ? library : albums);

	const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });
	const cmp = (a: string, b: string) => collator.compare(a, b);
	/** Dates read newest first; everything else A to Z. */
	const NEWEST_FIRST = new Set([
		'added',
		'date',
		'release_date',
		'original_date',
		'encoding_time',
		'tagging_time'
	]);

	/** An album's value for a sort: built-in fields, else the daemon's sort values. */
	function sortValue(a: Album, key: Sort): string | number | null {
		switch (key) {
			case 'added':
				return a.added_at;
			case 'album_artist':
				return a.artist;
			case 'album':
				return a.title;
			case 'date':
				return a.year;
			case 'genre':
				return a.genre;
			default:
				return a.sort_values?.[key] ?? null;
		}
	}

	/** Albums without a value go last; ties fall back to artist, year, title. */
	function comparator(key: Sort) {
		const sign = NEWEST_FIRST.has(key) ? -1 : 1;
		return (a: Album, b: Album) => {
			const x = sortValue(a, key);
			const y = sortValue(b, key);
			const xMissing = x === null || x === '';
			const yMissing = y === null || y === '';
			if (xMissing !== yMissing) return xMissing ? 1 : -1;
			if (!xMissing) {
				const c =
					typeof x === 'number' && typeof y === 'number' ? x - y : cmp(String(x), String(y));
				if (c) return sign * c;
			}
			return cmp(a.artist, b.artist) || (a.year ?? 0) - (b.year ?? 0) || cmp(a.title, b.title);
		};
	}

	let byId = $derived(new Map([...albums, ...library].map((a) => [a.id, a])));
	let haystack = $derived(
		new Map(
			[...albums, ...library].map((a) => [
				a.id,
				fold(`${a.title} ${a.artist} ${a.genre ?? ''} ${a.year ?? ''}`)
			])
		)
	);
	// Each collection is sorted once per sort order, so switching back and
	// forth is a lookup; searching then filters the sorted list.
	const sortCache = new WeakMap<Album[], Map<Sort, Album[]>>();
	function sorted(list: Album[], by: Sort) {
		let orders = sortCache.get(list);
		if (!orders) sortCache.set(list, (orders = new Map()));
		let out = orders.get(by);
		if (!out) orders.set(by, (out = list.slice().sort(comparator(by))));
		return out;
	}
	/** "Needs attention" filters rather than sorts: flagged albums, A to Z by artist. */
	let attentionView = $derived(activeSort === 'attention');
	let flagged = $derived(source.filter((a) => a.attention));
	/** The sort in use before Needs attention, to go back to. */
	let lastPlain = $state<Sort>();
	$effect(() => {
		if (sort !== 'attention') lastPlain = sort;
	});
	/** Where "Show all albums" leads: the sort before, else the menu's first plain one; none if there isn't one. */
	let plainSort = $derived(
		lastPlain && appearance.sorts.saved.includes(lastPlain)
			? lastPlain
			: appearance.sorts.saved.find((s) => s !== 'attention')
	);
	function showAll() {
		if (plainSort) sort = plainSort;
	}
	/** Albums the sort and search leave, in order; the sidebar's tag filters narrow them. */
	let searched = $derived.by(() => {
		const list = attentionView
			? sorted(source, 'album_artist').filter((a) => a.attention)
			: sorted(source, activeSort);
		// Setup screens use the search field for their own lists.
		const terms = isSetup(view) ? [] : fold(query).split(/\s+/).filter(Boolean);
		return terms.length
			? list.filter((a) => terms.every((t) => haystack.get(a.id)!.includes(t)))
			: list;
	});
	/** Tag values picked in the sidebar, kept per collection as their tags differ. */
	let filters = $state<Record<'inbox' | 'library', Filters>>({ inbox: {}, library: {} });
	let filterKinds = $derived(appearance.filters.saved);
	let filtered = $derived(picked(filters[browsing], filterKinds));
	let visible = $derived(
		filtered ? searched.filter(matcher(filters[browsing], filterKinds)) : searched
	);

	/** The album grid's width, for the Appearance screen's cover size preview. */
	let mainBox = $state<DOMRectReadOnly>();
	let gridWidth = $derived(Math.min(GRID_MAX_WIDTH, mainBox?.width ?? 0));
	/** The skeleton grid's cards match the real ones'. */
	let ghostWidth = $derived(gridLayout(gridWidth, appearance.savedCoverSize).minWidth);
	/** The top of the grid last browsed, for previewing cover sizes with real covers. */
	let previewAlbums = $derived.by(() => {
		const list = browsing === 'library' && library.length ? library : albums;
		return sorted(list, activeSort === 'attention' ? 'album_artist' : activeSort).slice(0, 12);
	});

	/** Card badge in the attention view: what's missing, and on how many tracks. */
	function attentionFlag(a: Album) {
		const at = a.attention;
		if (!at) return null;
		const names = at.missing.map((k) => setup.label(k));
		return {
			text: names.length === 1 ? names[0] : `${names.length} missing`,
			title: `Missing ${names.join(', ')} on ${at.tracks} of ${a.track_count} ${a.track_count === 1 ? 'track' : 'tracks'}`
		};
	}

	let openId = $derived(page.state.album ?? null);
	let openAlbum = $derived(openId ? (byId.get(openId) ?? null) : null);

	/** The open album's tag editor's edits: none, some not sent yet, or some being saved. */
	let draft = $state<Draft>('none');
	/**
	 * The sheet stays on the open album as it last showed it, though the grids
	 * have moved on. When `changed`, the album changed elsewhere (edited,
	 * renamed, imported, gone) while its tags had unsaved edits here, and the
	 * sheet keeps them until the user reloads it. Otherwise the sheet is on its
	 * way to the album's new id. `next` is the album's id now, if known. After
	 * a save refused for that, the sheet shows the album as it is now, with
	 * the edits on top (`kept`; see TagEditor).
	 */
	let held = $state<{
		from: string;
		next: string | null;
		album: Album;
		detail: Detail | null;
		changed: boolean;
		kept: TagDraft | null;
	} | null>(null);
	let holding = $derived(held?.from === openId ? held : null);
	/** Counts the sheet's openings (and closings): a Reload belongs to one. */
	let openings = 0;
	$effect(() => {
		void openId;
		openings++;
	});
	// `detail` is the open sheet's: once it closes, or another album opens, it's
	// dropped, and the next opening reads the album again. Moving on to the
	// album's new id (a rename, Reload) keeps what was read for it.
	$effect(() => {
		const id = openId;
		untrack(() => {
			if (detail && detail.id !== id) detail = null;
			if (held && held.from !== id) held = null;
		});
	});
	/** What the album sheet shows. */
	let sheetAlbum = $derived(holding?.album ?? openAlbum);
	let sheetDetail = $derived(
		holding ? holding.detail : detail?.id === sheetAlbum?.id ? detail : null
	);

	// Load the open album's tracks (again after signing back in, if that failed).
	$effect(() => {
		void openId;
		void session.generation;
		untrack(() => enqueue({ kind: 'open' }));
	});

	$effect(() => {
		document.documentElement.style.overflow = sheetAlbum || importing ? 'hidden' : '';
	});

	// Room the player takes at the bottom of the window, for bars that float above it.
	$effect(() => {
		document.documentElement.style.setProperty(
			'--dock',
			player.current ? 'calc(var(--player-h) + 12px)' : '0px'
		);
	});

	const reducedMotion = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

	/** Run a DOM update as a view transition morphing `from` into the cover `to` finds after it. */
	async function morph(
		from: HTMLElement | null,
		update: () => Promise<void>,
		to: () => HTMLElement | null
	) {
		if (!from || !document.startViewTransition || reducedMotion()) return update();
		from.style.viewTransitionName = 'album-cover';
		const vt = document.startViewTransition(async () => {
			from.style.viewTransitionName = '';
			await update();
		});
		follow(vt, to);
		await vt.updateCallbackDone.catch(() => {});
	}

	/**
	 * Keep the end of the cover's path on `landing` when the page or the sheet scrolls
	 * during the transition. Firefox fixes that end when the transition starts, so the
	 * cover flew to where its card had been and jumped when it finished. The spec (and
	 * Chrome) moves it with the element, so it's only moved here if, a frame after the
	 * first scroll that moves the cover, the browser hasn't moved it itself.
	 */
	function follow(vt: ViewTransition, landing: () => HTMLElement | null) {
		vt.ready
			.then(() => {
				const effect = document
					.getAnimations()
					.map((a) => a.effect)
					.find(
						(e): e is KeyframeEffect =>
							e instanceof KeyframeEffect &&
							e.pseudoElement === '::view-transition-group(album-cover)'
					);
				const end = () => effect?.getKeyframes().find((k) => k.offset === 1);
				const fixed = end()?.transform;
				const el = landing();
				if (!effect || typeof fixed !== 'string' || !el) return;
				const start = el.getBoundingClientRect();
				// Who moves the end: found out at the first scroll that moves the cover.
				let mover: 'unknown' | 'checking' | 'us' = 'unknown';
				const move = () => {
					const r = el.getBoundingClientRect();
					const frames = effect.getKeyframes();
					const last = frames.find((k) => k.offset === 1)!;
					last.transform = `translate(${r.left - start.left}px, ${r.top - start.top}px) ${fixed}`;
					effect.setKeyframes(frames);
				};
				const stop = () => removeEventListener('scroll', onscroll, { capture: true });
				const onscroll = () => {
					// The grid drops cards scrolled far out of view, and a detached card
					// measures 0×0 at 0,0: keep the last end it had.
					if (!el.isConnected) return;
					if (mover === 'us') return move();
					if (mover === 'checking') return;
					const r = el.getBoundingClientRect();
					if (r.left === start.left && r.top === start.top) return;
					mover = 'checking';
					// Two frames: the browser updates the end after this one's callbacks.
					requestAnimationFrame(() =>
						requestAnimationFrame(() => {
							if (end()?.transform !== fixed) return stop();
							mover = 'us';
							if (el.isConnected) move();
						})
					);
				};
				addEventListener('scroll', onscroll, { capture: true, passive: true });
				vt.finished.finally(stop);
			})
			.catch(() => {});
	}

	function gridCover(id: string): HTMLElement | null {
		const el = document.querySelector<HTMLElement>(`[data-album="${id}"] .cover`);
		if (!el) return null;
		const r = el.getBoundingClientRect();
		return r.bottom > 0 && r.top < innerHeight ? el : null;
	}

	async function open(album: Album, cover: HTMLElement | null = gridCover(album.id)) {
		if (openId === album.id) return;
		await morph(
			cover,
			async () => {
				// Behind the sheet the page is blurred with a filter, which would lay the
				// fixed drawer out against the page instead of the window.
				drawerOpen = false;
				// Shallow: a history entry with the album in page.state, no navigation.
				await goto(page.url.href, { state: { album: album.id }, shallow: true });
				await tick();
			},
			() => document.querySelector<HTMLElement>('.hero-cover')
		);
	}

	function back(): Promise<void> {
		return new Promise((resolve) => {
			const done = () => tick().then(() => resolve());
			const timer = setTimeout(done, 400);
			addEventListener('popstate', () => (clearTimeout(timer), done()), { once: true });
			history.back();
		});
	}

	async function close() {
		const id = openId;
		if (!id) return;
		const heroCover = document.querySelector<HTMLElement>('.hero-cover');
		const target = gridCover(id);
		if (!target || !document.startViewTransition || reducedMotion()) return back();
		const vt = document.startViewTransition(async () => {
			await back();
			// History can go back to another album, whose sheet stays: then its
			// cover is the end.
			if (sheetAlbum) return;
			// Back at the grid, the cover lands on its card, and the sheet fading
			// out leaves its own cover to the moving snapshot (see AlbumDetail).
			// Now, not on the fade's outrostart, which Svelte sends a frame later.
			document.querySelector('.sheet-wrap')?.classList.add('closing');
			target.style.viewTransitionName = 'album-cover';
		});
		heroCover?.style.setProperty('view-transition-name', 'album-cover');
		follow(vt, () => (sheetAlbum ? document.querySelector<HTMLElement>('.hero-cover') : target));
		vt.finished.finally(() => (target.style.viewTransitionName = ''));
	}

	async function playAlbum(album: Album) {
		try {
			const d = detail?.id === album.id ? detail : await api.album(album.id);
			player.playAlbum(album, d.tracks, 0);
		} catch {
			toast('Could not load album', 'error');
		}
	}

	function onsaved(result: TagWriteResult, id: string) {
		const from = openId;
		// Saved over whatever changed elsewhere: the sheet shows the album as
		// saved at once, so edits begun after Save stay. What came meanwhile is
		// checked after it (see enqueue).
		saves++;
		// Answered once the sheet that saved has closed (or shows another album):
		// only the grids show it.
		if (sheetDetail?.id !== id) return loadAlbums();
		// One object for the answer wherever the sheet shows it: the editor is
		// keyed on it, and starting over would lose edits made since.
		if (result.album) detail = result.album;
		const saved = result.album ? detail : null;
		// If the album has left the grids meanwhile (renamed elsewhere, say), the
		// sheet stays on it, quietly, until the notice saying where it went.
		held =
			holding && !openAlbum && sheetAlbum
				? { ...holding, album: sheetAlbum, detail: saved, changed: false, kept: null }
				: null;
		const grids = loadAlbums();
		if (!from || result.album_id === from) return grids;
		// A new title: the sheet stays while the grids catch up with the new id.
		if (sheetAlbum)
			held = {
				from,
				next: result.album_id,
				album: sheetAlbum,
				detail: saved,
				changed: false,
				kept: null
			};
		enqueue({ kind: 'moved', from, to: result.album_id }, true);
		return grids;
	}

	let lastAlbums = 0;
	async function fetchAlbums() {
		try {
			const [inbox, managed] = await Promise.all([api.albums('inbox'), api.albums('library')]);
			// The open album has left the grids (renamed, imported or removed
			// elsewhere) while its tags have unsaved edits, or while a notice
			// saying where it went waits its turn: the sheet stays on it.
			const gone = (id: string) =>
				!inbox.some((a) => a.id === id) && !managed.some((a) => a.id === id);
			const moving = !!openId && renamedLater(openId);
			// During a save, or when moving, it stays without a word.
			if (
				openId &&
				(draft !== 'none' || moving) &&
				!holding &&
				openId !== importingHere &&
				gone(openId)
			)
				hold(null, draft === 'draft');
			[albums, library] = [inbox, managed];
			lastAlbums = Date.now();
		} catch {
			toast('Could not reach coversyncd', 'error');
		}
		loaded = true;
	}
	let albumsLoading: Promise<void> | null = null;
	let albumsNext: Promise<void> | null = null;
	/**
	 * Reload both grids. Asked again while a reload runs, which may have been
	 * answered before the change, one more follows it (just one, however often
	 * it's asked), so the albums shown last are always the newest.
	 */
	function loadAlbums(): Promise<void> {
		if (!albumsLoading)
			return (albumsLoading = fetchAlbums().finally(() => (albumsLoading = null)));
		return (albumsNext ??= albumsLoading.then(() => {
			albumsNext = null;
			return loadAlbums();
		}));
	}

	// New rules or folder structure change which albums need attention.
	$effect(() => {
		if (setup.version + inboxSetup.version > 0) untrack(() => loadAlbums());
	});

	/** Bumped whenever the visible Inbox changes (a scan finished, files were unhidden). */
	let dataGeneration = $state(0);

	async function inboxChanged() {
		await loadAlbums();
		dataGeneration++;
	}
	/** A scan ended: show what it found. */
	function scanFinished() {
		dataGeneration++;
		// The Inbox folder may have come or gone; the empty Inbox's message says which.
		inboxSetup.refresh();
		return loadAlbums();
	}

	/** While a scan runs, the grids reload about this often, so albums appear as it reads them. */
	const SCAN_RELOAD_MS = 4000;
	/**
	 * The daemon's change notices are arriving, so polling is only a safety
	 * net. Without them, scans are polled for as before.
	 */
	let live = false;
	const SAFETY_POLL_MS = 60_000;
	/** Counts pushed scan ends, so a poll answered after one doesn't end the scan again. */
	let scanFinishes = 0;
	/** The scan's progress as last pushed, which a poll answered before it mustn't undo. */
	let pushedScan: Status['scan'] | null = null;
	let pollTimer: ReturnType<typeof setTimeout>;
	async function poll() {
		clearTimeout(pollTimer);
		// Signed out: signing back in polls again.
		if (session.state === 'out') return;
		try {
			const wasRunning = status?.scan.running ?? false;
			const finishes = scanFinishes;
			const pushed = pushedScan;
			const fresh = await api.status();
			status = pushedScan && pushedScan !== pushed ? { ...fresh, scan: pushedScan } : fresh;
			// A scan_finished that came meanwhile has seen to the end itself.
			if (wasRunning && !status.scan.running && finishes === scanFinishes) await scanFinished();
			else if (status.scan.running && Date.now() - lastAlbums > SCAN_RELOAD_MS) await loadAlbums();
		} catch {}
		// Only one poll waits at a time, even when two ran at once.
		clearTimeout(pollTimer);
		pollTimer = setTimeout(poll, live ? SAFETY_POLL_MS : status?.scan.running ? 1500 : 10000);
	}

	/** How long the first reads wait for the stream to open. */
	const OPEN_WAIT_MS = 1500;
	let readsStarted = false;
	let startReads = () => {};

	/**
	 * The stream first opened. The first reads start only now, so a change
	 * made before them is in them, and one made after comes as a notice. If
	 * they couldn't wait for it (see `OPEN_WAIT_MS`), they may have missed some.
	 */
	function onfirst() {
		if (readsStarted) resync();
		else startReads();
	}

	function onlive(up: boolean) {
		if (up === live) return;
		live = up;
		// Down: poll again straight away. Up: the safety poll takes over after a resync.
		if (!up) poll();
		// The server answers: the app can be found now, if it couldn't before.
		else findApp();
	}

	/**
	 * In the desktop app, which notifies of what finishes while its window is
	 * in the background, the page hears of it while hidden too.
	 */
	async function findApp() {
		await desktopApp.detect();
		if (desktopApp.has('notify')) listening?.stayOpen();
	}

	/** An album as a notification names it. */
	const named = (id: string) => {
		const album = albums.find((a) => a.id === id);
		return album ? `${album.artist} · ${album.title}` : '';
	};

	/** The open album's id as of the latest notice (a held sheet's album may have a new one). */
	const openNow = () => holding?.next ?? openId;

	/** An album: `null` if it's gone (404), `undefined` if that couldn't be told. */
	const fetchAlbum = (id: string) =>
		api.album(id).catch((e) => (e instanceof ApiError && e.status === 404 ? null : undefined));

	/** Once the grid reloads asked for so far are done. */
	const gridsSettled = (): Promise<void> => albumsNext ?? albumsLoading ?? Promise.resolve();

	/**
	 * Keep the sheet on the open album as it shows it now, with its unsaved
	 * edits, and say it changed elsewhere (unless not `changed`, yet). `next` is
	 * the album's id now, if known.
	 */
	function hold(next: string | null, changed = true) {
		if (!openId || !sheetAlbum) return;
		held = {
			from: openId,
			next: next ?? holding?.next ?? null,
			album: sheetAlbum,
			detail: sheetDetail,
			changed,
			kept: holding?.kept ?? null
		};
	}

	/**
	 * What can change the open album's sheet once it's open: an album opened,
	 * a notice, a resync, this page's save giving it a new title, and Reload.
	 * They run one at a time, in order, each going by the sheet as it is when
	 * its turn comes, so none lands over another. While this page saves, they
	 * wait (but for opening an album) until it's answered.
	 */
	type Job =
		| { kind: 'open' }
		| { kind: 'notice'; id: string; previous: string | null; version?: string | null }
		| { kind: 'resync' }
		| { kind: 'moved'; from: string; to: string }
		| { kind: 'reload'; sheet: string; opening: number }
		| {
				kind: 'refused';
				saved: string;
				current: Detail | null;
				kept: () => TagDraft | null;
		  };
	let jobs: Job[] = [];
	let running = false;
	let wake: (() => void) | null = null;
	/** Bumped by each save that worked, whose answer is shown at once (see onsaved). */
	let saves = 0;

	function enqueue(job: Job, first = false) {
		// A later resync, reload or open makes one still waiting moot.
		if (job.kind === 'resync' || job.kind === 'reload' || job.kind === 'open')
			jobs = jobs.filter((j) => j.kind !== job.kind);
		if (first) jobs.unshift(job);
		else jobs.push(job);
		if (running) wake?.();
		else void run();
	}

	async function run() {
		running = true;
		try {
			while (jobs.length) {
				// A save's own answers (moved, refused) go on while it's answered.
				const job =
					draft === 'saving'
						? jobs.find((j) => j.kind === 'open' || j.kind === 'moved' || j.kind === 'refused')
						: jobs[0];
				if (!job) {
					await new Promise<void>((resolve) => (wake = resolve));
					wake = null;
					continue;
				}
				jobs = jobs.filter((j) => j !== job);
				// An answer that may be from before a save made meanwhile is asked again.
				if (!(await perform(job))) jobs.unshift(job);
				// The sheet, and its editor, settle before the next.
				await tick();
			}
		} finally {
			running = false;
		}
	}

	/** A notice still waiting renames album `id`. */
	const renamedLater = (id: string) =>
		jobs.some((j) => j.kind === 'notice' && j.previous === id && j.id !== id);

	/** Returns false if the job is to run again. */
	async function perform(job: Job): Promise<boolean> {
		try {
			const now = openNow();
			switch (job.kind) {
				case 'open':
					await loadOpen();
					return true;
				case 'notice':
					if (now === null || (now !== job.id && now !== job.previous)) return true;
					// A later notice says where the album went: that one shows it, and
					// now matches the sheet as this one would have.
					if (renamedLater(job.id)) {
						if (job.id !== now)
							jobs = jobs.map((j) =>
								j.kind === 'notice' && j.previous === job.id ? { ...j, previous: now } : j
							);
						return true;
					}
					// The sheet shows that version already (a save of this page's,
					// answered or refused): nothing to show.
					if (job.version && sheetDetail?.version === job.version) return true;
					// Held: Reload shows it as it is; a new id is noted for that.
					if (holding?.changed) {
						if (job.id !== now) hold(job.id);
						return true;
					}
					return await check(job.id);
				case 'resync':
					if (now === null || holding?.changed || renamedLater(now)) return true;
					return await check(now, true);
				case 'moved':
					// The sheet moves once the grids have the new id.
					if (openId !== job.from) return true;
					await gridsSettled();
					// Renamed again elsewhere: the notice saying so takes the sheet there.
					if (openId === job.from && !renamedLater(job.to))
						await goto(page.url.href, { state: { album: job.to }, shallow: true, replace: true });
					return true;
				case 'reload':
					await reload(job.sheet, job.opening);
					return true;
				case 'refused':
					refused(job);
					return true;
			}
		} catch (e) {
			console.error(e);
			return true;
		}
	}

	/** An album was opened: load its tracks. */
	async function loadOpen() {
		const id = openId;
		if (!id || detail?.id === id) return;
		const d = await fetchAlbum(id);
		if (openId !== id) return;
		if (d) detail = d;
		else toast('Could not load album', 'error');
	}

	/**
	 * The open album changed, and is now `to` (a new id or not). Fetch it, then
	 * go by the sheet as it is once that's answered: with no unsaved edits,
	 * show it, under the new id once the grids have it; with some, keep them
	 * and say it changed, if it did. An answer that couldn't be had changes
	 * nothing. Returns false if a save of this page's was answered meanwhile,
	 * as the album may have been read before it. `settle`: a resync, with no
	 * notice to come; otherwise a new id already gone has been renamed again,
	 * and the notice saying so moves the sheet on.
	 */
	async function check(to: string, settle = false): Promise<boolean> {
		const from = openId;
		if (!from || !sheetAlbum) return true;
		// A new id is a change: with edits, say so at once.
		if (to !== from && draft === 'draft') {
			hold(to);
			return true;
		}
		// Without, the sheet stays as it is while it moves.
		if (to !== from)
			held = {
				from,
				next: to,
				album: sheetAlbum,
				detail: sheetDetail,
				changed: false,
				kept: null
			};
		const before = saves;
		const [fresh] = await Promise.all([fetchAlbum(to), to !== from ? gridsSettled() : null]);
		if (openId !== from) return true;
		if (saves !== before) return false;
		// A notice waiting renames it again: that one shows it.
		if (renamedLater(to)) return true;
		// Not known (the daemon away, signed out): a later resync asks again.
		if (fresh === undefined) return true;
		// Unchanged (this page's own save, come back): the editor and its edits
		// are left be. Its version says, as a save's does (see refused).
		if (fresh && to === from && fresh.version === sheetDetail?.version) return true;
		if (draft !== 'none') {
			// A save begun meanwhile says so if it fails (see ondraft).
			hold(fresh ? to : null, draft === 'draft');
			return true;
		}
		if (!fresh) {
			if (settle || to === from) hold(null);
			return true;
		}
		if (to === from) held = null;
		detail = fresh;
		if (openId !== to)
			await goto(page.url.href, { state: { album: to }, shallow: true, replace: true });
		return true;
	}

	/** Reload, pressed on the sheet opened as `sheet`: drop its edits, and show the album as it is now. */
	async function reload(sheet: string, opening: number) {
		const h = holding;
		const before = saves;
		// The sheet it was asked on, still held, or it's moot: closed (or
		// reopened) since, or saved over.
		const current = () => openId === sheet && openings === opening && holding === h;
		if (!h || !current()) return;
		const to = h.next ?? sheet;
		const [fresh] = await Promise.all([fetchAlbum(to), loadAlbums()]);
		if (!current() || saves !== before) return;
		if (fresh === undefined) {
			toast('Couldn’t reload the album. Try again in a moment.', 'error');
			return;
		}
		if (fresh === null) {
			// Its new id isn't known when the notice was missed (a hidden tab, the stream down).
			toast('This album was renamed, imported or removed elsewhere', 'info');
			held = null;
			return close();
		}
		// A new copy of the album: the editor starts over, without the edits.
		detail = fresh;
		held =
			to === sheet
				? null
				: { from: sheet, next: to, album: h.album, detail, changed: false, kept: null };
		if (openId !== to)
			await goto(page.url.href, { state: { album: to }, shallow: true, replace: true });
	}

	function ondraft(next: Draft) {
		const was = draft;
		draft = next;
		if (was !== 'saving' || next === 'saving') return;
		// The save failed, and the album moved meanwhile (the sheet stayed for it): say so now.
		if (next === 'draft' && holding && !holding.changed) hold(holding.next);
		// What came during the save goes on, after its answer.
		wake?.();
	}

	/**
	 * A save was refused because the album had changed elsewhere since the
	 * sheet's copy: `current` is the album as it is now (null: gone), `kept`
	 * reads the edits from the editor that saved (null once it has closed), and
	 * `id` is the album's id the save was sent for. It goes through the queue,
	 * ahead of what came meanwhile, so no notice lands over it.
	 */
	function onrefused(current: Detail | null, kept: () => TagDraft | null, id: string) {
		// The sheet has moved on (closed, or another album): only the toast says so.
		if (sheetDetail?.id !== id) return;
		enqueue({ kind: 'refused', saved: id, current, kept }, true);
	}

	/**
	 * Hold the sheet on the album as it is now, with the edits of the editor
	 * that saved on top, as they are now, and the fields changed since marked
	 * (see TagEditor). That editor closed (the sheet with it, or left for the
	 * Tracks view), it's moot. Notices about versions the sheet shows from now
	 * on change nothing. Gone, the sheet stays as it is, held.
	 */
	function refused(job: Extract<Job, { kind: 'refused' }>) {
		if (!openId || sheetDetail?.id !== job.saved) return;
		const kept = job.kept();
		if (!kept) return;
		if (!job.current) return hold(null);
		held = {
			from: openId,
			next: job.current.id === openId ? null : job.current.id,
			album: job.current,
			detail: job.current,
			changed: true,
			kept
		};
		void loadAlbums();
	}

	/** The tag editor's edits were dropped: those kept from a refused save go too. */
	function ondiscard() {
		if (holding?.kept) held = { ...holding, kept: null };
	}

	/** Drop the held sheet's unsaved edits and show its album as it is now. */
	function reloadHeld() {
		if (openId) enqueue({ kind: 'reload', sheet: openId, opening: openings });
	}

	/** An album was edited, here or elsewhere: show it as it is now. */
	function albumChanged({ id, previous_id, collection, version }: Notices['album_changed']) {
		// The Inbox's tag coverage, which Library setup shows, may have changed.
		if (collection === 'inbox') dataGeneration++;
		loadAlbums();
		enqueue({ kind: 'notice', id, previous: previous_id, version });
	}

	/** Reload everything: notices were missed, or the stream has just come back. */
	function resync() {
		poll();
		appearance.refresh();
		syncSetup.refresh();
		setup.refresh();
		inboxSetup.refresh();
		dataGeneration++;
		loadAlbums();
		enqueue({ kind: 'resync' });
	}

	/** What each change notice reloads. */
	const notices: NoticeHandlers = {
		scan(scan) {
			pushedScan = scan;
			// Until the first poll answers, it's kept for that (see poll).
			if (status) status = { ...status, scan };
			// Albums appear in the grids as the scan reads their tags.
			if (!albumsLoading && Date.now() - lastAlbums > SCAN_RELOAD_MS) loadAlbums();
		},
		scan_finished(scan) {
			pushedScan = scan;
			scanFinishes++;
			if (status) status = { ...status, scan };
			scanFinished();
			const found = `${count(scan.albums)} ${scan.albums === 1 ? 'album' : 'albums'}`;
			const unread = scan.errors
				? `, ${count(scan.errors)} ${scan.errors === 1 ? 'file' : 'files'} couldn’t be read`
				: '';
			notify('Scan finished', found + unread, 'scan');
		},
		imported({ album_id, inbox_id }) {
			// By its Inbox entry, which the reload below takes away.
			notify('Imported to the Library', named(inbox_id), `import-${album_id}`);
			// Copies hide the Inbox files, which Inbox setup counts.
			inboxSetup.refresh();
			inboxChanged();
			// The Inbox album open moves to the Library (an import made here
			// closes its sheet itself); a Library album open may have more tracks.
			enqueue({
				kind: 'notice',
				id: album_id,
				previous: inbox_id === importingHere ? null : inbox_id
			});
		},
		album_changed: albumChanged,
		lookup_finished({ album_id, state }) {
			notify(
				state === 'done' ? 'Suggestions are ready' : 'Looking for suggestions failed',
				named(album_id),
				`lookup-${album_id}`
			);
		},
		setup_saved({ setup: saved }) {
			// Each can change which albums need attention, or what sorts and filters show.
			if (saved === 'library') setup.refresh();
			else if (saved === 'inbox') {
				// Hidden files may be back in the Inbox.
				inboxSetup.refresh();
				dataGeneration++;
			} else if (saved === 'appearance') appearance.refresh();
			else return syncSetup.refresh();
			loadAlbums();
		},
		resync
	};

	// Right-click menu on album cards.
	let menu = $state<{ album: Album; x: number; y: number } | null>(null);
	let menuItems = $derived.by((): MenuEntry[] => {
		const a = menu?.album;
		if (!a) return [];
		const items: MenuEntry[] = [
			{ label: 'Open', icon: PanelTopOpen, run: () => open(a) },
			{ label: 'Play', icon: Play, run: () => playAlbum(a) }
		];
		if (a.collection === 'inbox')
			items.push('separator', {
				label: 'Import to Library',
				icon: FolderInput,
				primary: true,
				run: () => startImport(a)
			});
		return items;
	});

	// Import: check the album against the library rules; import straight away
	// when nothing is missing, otherwise open the editor with the gaps marked.
	/** The import editor: the plan plus any edits made before it opened. */
	let importing = $state<{ album: Album; plan: ImportPlan; edits: ImportEdits } | null>(null);

	/** Point toasts about setup problems at the screen that fixes them. */
	function fixAction(problem: string): ToastAction | undefined {
		if (/library folder|folder structure/i.test(problem))
			return { label: 'Library setup', run: () => setView('library-setup') };
		if (/Inbox folder|Move/i.test(problem))
			return { label: 'Inbox setup', run: () => setView('inbox-setup') };
	}

	/** `edits` come from the album's tag editor, if the import started there. */
	async function startImport(album: Album, edits: ImportEdits = {}) {
		const checking = toast(`Checking “${album.title}”…`, 'progress', 0);
		let plan: ImportPlan;
		try {
			plan = await api.importPlan(album.id, edits);
		} catch (e) {
			toast(e instanceof ApiError ? e.message : 'Could not check the album', 'error', 6000);
			return;
		} finally {
			dismiss(checking);
		}
		if (plan.failing.length || plan.disagree.length || plan.art.missing || plan.suggest) {
			// The editor needs the tag catalog (labels, album-wide kinds) up front.
			await setup.load();
			importing = { album, plan, edits };
		} else if (plan.problems.length) {
			toast(plan.problems.join(' '), 'error', 8000, fixAction(plan.problems[0]));
		} else {
			await runImport(album, edits, plan.autofilled);
		}
	}

	/** The Inbox album this page is importing, whose sheet it closes itself. */
	let importingHere: string | null = null;
	/** Returns an error message, or null when the album was imported. */
	/** `autofilled`: kinds autofill sets, mentioned in the success toast. */
	async function runImport(
		album: Album,
		edits: ImportEdits,
		autofilled: string[] = [],
		art?: string,
		suggested = 0,
		musicbrainzArt?: string
	): Promise<string | null> {
		const working = toast(`Importing “${album.title}”…`, 'progress', 0);
		importingHere = album.id;
		try {
			const r = await api.importAlbum(album.id, edits, art, musicbrainzArt);
			importing = null;
			// The album leaves the Inbox; close its sheet first so history stays tidy.
			if (openId === album.id) await close();
			await loadAlbums();
			dataGeneration++;
			const how = r.mode === 'copy' ? 'copied' : 'moved';
			const filled = autofilled.length
				? ` · filled in ${autofilled.map((k) => setup.label(k)).join(', ')}`
				: '';
			const placed = r.art ? ` · ${r.art}` : '';
			const taken = [
				suggested && `${suggested} ${suggested === 1 ? 'tag' : 'tags'}`,
				musicbrainzArt && 'the cover'
			].filter(Boolean);
			const fromMb = taken.length ? ` · ${taken.join(' and ')} from MusicBrainz` : '';
			toast(
				`Imported “${album.title}”: ${r.files} ${r.files === 1 ? 'file' : 'files'} ${how}${filled}${fromMb}${placed}`,
				'success',
				7000,
				{ label: 'Show', run: () => showInLibrary(r.album_id) }
			);
			return null;
		} catch (e) {
			const message = e instanceof ApiError ? e.message : 'The import failed';
			if (!importing) toast(message, 'error', 8000, fixAction(message));
			return message;
		} finally {
			importingHere = null;
			dismiss(working);
		}
	}

	async function showInLibrary(id: string) {
		await setView('library');
		await tick();
		const album = byId.get(id);
		if (album) open(album);
	}

	async function rescan() {
		await api.rescan().catch(() => toast('Rescan failed to start', 'error'));
		// The folder may have been mounted (or unmounted) since the page loaded.
		inboxSetup.refresh();
		setTimeout(poll, 300);
	}

	/** The key went to an element matching `selector` (the target may be the window). */
	function within(e: KeyboardEvent, selector: string) {
		return e.target instanceof Element && !!e.target.closest(selector);
	}
	const typing = (e: KeyboardEvent) => within(e, 'input, textarea, [contenteditable]');
	function onkeydown(e: KeyboardEvent) {
		// The login view is up: keys are its own, and Escape mustn't close the
		// album (and its unsaved edits) under it.
		if (session.state === 'out') return;
		if (e.key === 'Escape' && openId) {
			e.preventDefault();
			close();
		} else if (e.key === 'Escape' && narrow && drawerOpen) {
			drawerOpen = false;
		} else if (e.key === 'Escape' && isSetup(view) && !typing(e)) {
			setView(browsing);
		} else if (typing(e) || e.ctrlKey || e.metaKey || e.altKey) {
			return;
		} else if (e.key === '/') {
			e.preventDefault();
			search?.focus();
		} else if (e.key === ' ' && !within(e, 'button, [role=slider]')) {
			e.preventDefault();
			player.toggle();
		}
	}

	function clearFilters() {
		filters[browsing] = {};
	}

	/**
	 * What the page needs from the daemon. A setup already loaded (by its
	 * screen, say) is read again, keeping unsaved edits.
	 */
	function loadAll() {
		return Promise.all([
			poll(),
			appearance.refresh(),
			// Whether the sidebar shows its Sync section, and the devices it lists.
			syncSetup.refresh(),
			devices.load(),
			// Whether an Inbox folder is set, for the empty Inbox's message.
			inboxSetup.refresh(),
			// Tag labels for attention badges.
			setup.refresh(),
			loadAlbums()
		]);
	}

	// Signed back in after the session ended: whatever was asked for meanwhile failed.
	$effect(() => {
		if (session.generation > 0) untrack(loadAll);
	});

	let listening: Listening | undefined;
	// Signed out, the stream would only be refused: it waits, and catches up
	// once signed back in.
	$effect(() => {
		const out = session.state === 'out';
		untrack(() => (out ? listening?.pause() : listening?.resume()));
	});

	onMount(() => {
		// Listen first, then read: see onfirst.
		listening = listen(notices, onlive, onfirst);
		findApp();
		const wait = setTimeout(() => startReads(), OPEN_WAIT_MS);
		startReads = () => {
			if (readsStarted) return;
			readsStarted = true;
			clearTimeout(wait);
			loadAll();
		};
		const mq = matchMedia(NARROW);
		const onchange = () => {
			narrow = mq.matches;
			drawerOpen = false;
		};
		mq.addEventListener('change', onchange);
		return () => {
			clearTimeout(wait);
			listening?.stop();
			clearTimeout(pollTimer);
			mq.removeEventListener('change', onchange);
		};
	});
</script>

<svelte:window {onkeydown} />

<!-- The page under an open album: inert, and blurred (see .behind). -->
<div class="page" class:behind={!!sheetAlbum} inert={!!sheetAlbum}>
	<TopBar
		bind:query
		bind:search
		{view}
		counts={{ inbox: loaded ? albums.length : null, library: loaded ? library.length : null }}
		scan={status?.scan ?? null}
		shown={visible.length}
		total={source.length}
		sidebarOpen={!collapsed}
		{filtered}
		onview={setView}
		ontogglesidebar={toggleSidebar}
		onrescan={rescan}
	>
		{#snippet sorts()}
			<SortMenu
				sorts={appearance.sorts.saved}
				active={activeSort}
				label={(id) => appearance.label(id)}
				counts={{ attention: flagged.length }}
				onsort={(id) => (sort = id)}
				disabled={isSetup(view)}
			/>
		{/snippet}
	</TopBar>

	<div class="shell">
		<Sidebar
			kinds={filterKinds}
			label={(id) => appearance.label(id)}
			albums={searched}
			bind:filters={filters[browsing]}
			{collapsed}
			{view}
			sync={syncSetup.loaded && syncSetup.savedEnabled}
			dirty={{
				'inbox-setup': inboxSetup.changes > 0,
				'library-setup': setup.changes > 0,
				appearance: appearance.changes > 0,
				'sync-setup': syncSetup.changes > 0
			}}
			onview={setView}
		/>

		<main bind:contentRect={mainBox}>
			{#key view}
				<div class="view" in:fly={{ x: 28 * direction, duration: 420, easing: cubicOut }}>
					{#if attentionView && !isSetup(view) && loaded && appearance.settled && source.length}
						<!-- A filter kept across visits, so it's always on show while it applies. -->
						<div class="attention-note" transition:slide={{ duration: 220 }}>
							<CircleAlert size={15} strokeWidth={2.2} />
							<span>
								Only albums that need attention: <b class="tabular">{count(flagged.length)}</b> of
								{count(source.length)}
							</span>
							{#if plainSort}
								<button class="link" onclick={showAll}>Show all albums</button>
							{/if}
						</div>
					{/if}
					{#if view === 'library-setup'}
						<LibrarySetup
							{query}
							back={setupBack}
							scanning={status?.scan.running ?? false}
							generation={dataGeneration}
						/>
					{:else if view === 'appearance'}
						<AppearanceSetup
							{query}
							back={setupBack}
							onsaved={loadAlbums}
							{gridWidth}
							albums={previewAlbums}
						/>
					{:else if view === 'inbox-setup'}
						<InboxSetup
							back={setupBack}
							onchanged={inboxChanged}
							onrescan={() => setTimeout(poll, 300)}
						/>
					{:else if view === 'sync-setup'}
						<SyncSetup back={setupBack} />
					{:else if !loaded || !appearance.settled}
						<div class="grid" aria-busy="true" style:--cover-min="{ghostWidth}px">
							{#each { length: 18 } as _, i (i)}
								<div class="ghost" style:--i={i}><span></span><b></b><b></b></div>
							{/each}
						</div>
					{:else if view === 'library' && library.length === 0}
						<div class="empty">
							<div class="fan" aria-hidden="true">
								<span></span><span></span><span><LibraryBig size={30} strokeWidth={1.4} /></span>
							</div>
							<h2>Your library is empty</h2>
							<p>Albums you bring in from the Inbox will be organized and managed here.</p>
							<div class="empty-actions">
								<button class="btn primary" onclick={() => setView('library-setup')}>
									<Settings2 size={16} /> Set up rules
								</button>
								<button class="btn" onclick={() => setView('inbox')}>
									<Inbox size={16} /> Browse the Inbox
								</button>
							</div>
						</div>
					{:else if source.length === 0}
						<div class="empty">
							<span class="empty-icon"><Disc3 size={44} strokeWidth={1.2} /></span>
							{#if status?.scan.running}
								<h2>Scanning for music…</h2>
								<p>Albums appear in the Inbox as their tags are read.</p>
							{:else if inboxSetup.loaded && !inboxSetup.saved.folder}
								<h2>No Inbox folder yet</h2>
								<p>Choose the folder coversync scans for music to bring into the Library.</p>
								<div class="empty-actions">
									<button class="btn primary" onclick={() => setView('inbox-setup')}>
										<FolderCog size={16} /> Choose a folder
									</button>
								</div>
							{:else if inboxSetup.folderStatus && !inboxSetup.folderStatus.exists}
								<h2>The Inbox folder can't be found</h2>
								<p>
									<code>{inboxSetup.saved.folder}</code> isn't there right now. Mount it and rescan, or
									choose another folder.
								</p>
								<div class="empty-actions">
									<button class="btn primary" onclick={rescan}>
										<RefreshCw size={16} /> Rescan
									</button>
									<button class="btn" onclick={() => setView('inbox-setup')}>
										<FolderCog size={16} /> Inbox setup
									</button>
								</div>
							{:else}
								<h2>The Inbox is empty</h2>
								<p>No audio files were found in the Inbox folder.</p>
								<div class="empty-actions">
									<button class="btn primary" onclick={() => setView('inbox-setup')}>
										<FolderCog size={16} /> Inbox setup
									</button>
									<button class="btn" onclick={rescan}>
										<RefreshCw size={16} /> Rescan
									</button>
								</div>
							{/if}
						</div>
					{:else if visible.length === 0 && attentionView && !query.trim() && !filtered}
						<div class="empty">
							<span class="empty-ok"><CircleCheck size={44} strokeWidth={1.4} /></span>
							<h2>Nothing needs attention</h2>
							<p>Every album here has the tags the library rules and folder structure need.</p>
						</div>
					{:else if visible.length === 0 && filtered}
						<div class="empty">
							<h2>
								{attentionView
									? 'No albums needing attention match these filters'
									: 'No albums match these filters'}
							</h2>
							<p>
								{query.trim() ? `Nothing with these tags matches “${query}”.` : 'Try fewer tags.'}
							</p>
							<div class="empty-actions">
								<button class="btn" onclick={clearFilters}>
									<FilterX size={16} /> Clear filters
								</button>
							</div>
						</div>
					{:else if visible.length === 0}
						<div class="empty">
							<h2>
								{attentionView
									? `Nothing that needs attention matches “${query}”`
									: `Nothing matches “${query}”`}
							</h2>
							<p>Try an artist, album title, genre or year.</p>
						</div>
					{:else}
						<AlbumGrid
							albums={visible}
							size={appearance.savedCoverSize}
							onopen={open}
							onplay={playAlbum}
							onmenu={(a, x, y) => (menu = { album: a, x, y })}
							flag={attentionView ? attentionFlag : undefined}
						/>
					{/if}
				</div>
			{/key}
		</main>
	</div>
</div>

{#if narrow && drawerOpen}
	<button
		class="scrim"
		aria-label="Close sidebar"
		onclick={() => (drawerOpen = false)}
		transition:fade={{ duration: 260 }}
	></button>
{/if}

{#if sheetAlbum}
	<AlbumDetail
		album={sheetAlbum}
		detail={sheetDetail}
		allowWrites={sheetAlbum.collection === 'library' || (status?.allow_writes ?? false)}
		onclose={close}
		{onsaved}
		onimport={sheetAlbum.collection === 'inbox'
			? (edits) => startImport(sheetAlbum!, edits)
			: undefined}
		changed={holding?.changed ?? false}
		onreload={reloadHeld}
		{ondraft}
		kept={holding?.kept ?? null}
		{onrefused}
		{ondiscard}
	/>
{/if}

<PlayerBar onopen={(id) => byId.get(id) && open(byId.get(id)!)} />
{#if menu}
	<!-- Keyed so a right-click on another card places a fresh menu. -->
	{#key menu}
		<ContextMenu
			x={menu.x}
			y={menu.y}
			title={menu.album.title}
			items={menuItems}
			onclose={() => (menu = null)}
		/>
	{/key}
{/if}

{#if importing}
	<ImportDialog
		album={importing.album}
		plan={importing.plan}
		baseEdits={importing.edits}
		label={(id) => setup.label(id)}
		albumKinds={setup.albumKinds}
		notEditable={setup.notEditable}
		coreKinds={setup.kinds.filter((k) => k.group === 'core').map((k) => k.id)}
		kindOrder={setup.kinds.map((k) => k.id)}
		onclose={() => (importing = null)}
		onimport={(edits, art, suggested, mbArt) =>
			runImport(importing!.album, edits, importing!.plan.autofilled, art, suggested, mbArt)}
	/>
{/if}

<Toasts />

<style>
	/* Frosted behind an open album's sheet. A filter on the page rather than a
	   backdrop-filter on the sheet, which Firefox draws at full strength the
	   whole time the cover moves, and WebKitGTK draws again on every frame,
	   while the sheet scrolls. */
	.page.behind {
		filter: blur(18px) saturate(1.2);
	}
	/* While the cover moves the blur comes and goes with the sheet (see app.css). */
	:global(:root:active-view-transition) .page {
		transition: filter 280ms var(--ease-out);
	}
	/* Not captured on its own while it's blurred (Firefox, see TopBar): its snapshot would lose the blur. */
	.page.behind > :global(header) {
		view-transition-name: none;
	}
	.shell {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
	}
	main {
		/* Its own column even when the sidebar is a drawer, out of the grid's flow;
		   the auto column would shrink it to nothing, as the album grid has no
		   intrinsic width. */
		grid-column: 2;
		min-width: 0;
		/* Clear of the player while it's showing (--dock). */
		padding: 28px 28px calc(var(--dock, 0px) + 56px);
	}
	/* The attention view is a filter: say so above the grid, with the way out. */
	.attention-note {
		display: flex;
		align-items: center;
		gap: 10px;
		max-width: 1800px;
		margin: 0 auto 22px;
		padding: 8px 8px 8px 14px;
		border-radius: 12px;
		font-size: 13.5px;
		color: var(--text-2);
		background: var(--warn-surface);
		border: 1px solid var(--warn-border);
	}
	.attention-note :global(svg) {
		flex: none;
		color: var(--warn);
	}
	.attention-note span {
		min-width: 0;
	}
	.attention-note b {
		color: var(--text);
		font-weight: 600;
	}
	.attention-note .link {
		margin-left: auto;
		padding: 4px 10px;
		border-radius: 999px;
		font-size: 13px;
		font-weight: 600;
		color: var(--accent);
		white-space: nowrap;
		transition: background 140ms;
	}
	.attention-note .link:hover {
		background: color-mix(in oklab, var(--accent) 14%, transparent);
	}
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 25;
		background: rgb(0 0 0 / 0.45);
		backdrop-filter: blur(2px);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(var(--cover-min), 1fr));
		gap: 30px 22px;
		max-width: 1800px;
		margin: 0 auto;
	}
	.ghost {
		display: grid;
		gap: 8px;
		animation: fade-in 400ms both;
		animation-delay: calc(var(--i) * 30ms);
	}
	.ghost span,
	.ghost b {
		border-radius: var(--radius);
		background: linear-gradient(
				90deg,
				var(--surface) 0%,
				var(--surface-hover) 50%,
				var(--surface) 100%
			)
			0 0 / 200% 100%;
		animation: shimmer 1.4s linear infinite;
	}
	.ghost span {
		aspect-ratio: 1;
	}
	.ghost b {
		height: 12px;
		width: 80%;
		border-radius: 6px;
	}
	.ghost b:last-child {
		width: 55%;
	}
	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}
	@keyframes fade-in {
		from {
			opacity: 0;
		}
	}
	.empty {
		display: grid;
		justify-items: center;
		gap: 6px;
		padding: 18vh 24px;
		text-align: center;
		animation: fade-in 500ms both;
	}
	.empty h2 {
		margin: 8px 0 0;
		font-size: 22px;
		letter-spacing: -0.02em;
	}
	.empty p {
		margin: 0;
		max-width: 560px;
		color: var(--text-2);
	}
	.empty code {
		font-size: 13px;
		color: var(--text);
		overflow-wrap: anywhere;
	}
	.empty-actions {
		display: flex;
		flex-wrap: wrap;
		justify-content: center;
		gap: 10px;
		margin-top: 18px;
	}
	/* Three empty sleeves fanning out: the library waiting to be filled. */
	.fan {
		position: relative;
		width: 96px;
		height: 96px;
		margin-bottom: 22px;
	}
	.fan span {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		border-radius: 18px;
		border: 1.5px dashed var(--border-strong);
		background: var(--bg-raised);
		color: var(--text-3);
		animation: fan-out 900ms var(--ease-spring) 120ms both;
	}
	.fan span:nth-child(1) {
		--to: translateX(-40px) rotate(-12deg) scale(0.88);
		opacity: 0.8;
	}
	.fan span:nth-child(2) {
		--to: translateX(40px) rotate(12deg) scale(0.88);
		opacity: 0.8;
	}
	.fan span:nth-child(3) {
		--to: translateY(-2px);
		border-style: solid;
		border-color: color-mix(in oklab, var(--accent) 45%, transparent);
		color: var(--accent);
		box-shadow: 0 18px 40px -16px color-mix(in oklab, var(--accent) 50%, transparent);
		animation:
			fan-out 900ms var(--ease-spring) 120ms both,
			float 4s ease-in-out 1.1s infinite alternate;
	}
	@keyframes fan-out {
		from {
			transform: none;
		}
		to {
			transform: var(--to);
		}
	}
	@keyframes float {
		from {
			transform: var(--to);
		}
		to {
			transform: translateY(-8px);
		}
	}
	.empty-ok {
		color: var(--success);
		animation: pop 600ms var(--ease-spring) both;
	}
	@keyframes pop {
		from {
			opacity: 0;
			transform: scale(0.6);
		}
	}
	.empty-icon {
		color: var(--text-3);
		animation: spin 6s linear infinite;
	}
	@keyframes spin {
		to {
			rotate: 360deg;
		}
	}
	@media (max-width: 600px) {
		main {
			padding: 18px 16px calc(var(--dock, 0px) + 64px);
		}
		.grid {
			gap: 22px 14px;
		}
		.attention-note {
			margin-bottom: 16px;
		}
	}
</style>
