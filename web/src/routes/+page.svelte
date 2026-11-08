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
	import { matcher, picked, type Filters } from '#lib/filters.ts';
	import { listen, type Listening, type NoticeHandlers, type Notices } from '#lib/events.ts';
	import { count, fold } from '#lib/format.ts';
	import { GRID_MAX_WIDTH, gridLayout } from '#lib/grid.ts';
	import { player } from '#lib/player.svelte.ts';
	import { session } from '#lib/session.svelte.ts';
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
	import type { Draft } from '#lib/components/TagEditor.svelte';
	import { sameJson } from '#lib/same.ts';

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
	 * way to the album's new id. `next` is the album's id now, if known.
	 */
	let held = $state<{
		from: string;
		next: string | null;
		album: Album;
		detail: Detail | null;
		changed: boolean;
	} | null>(null);
	let holding = $derived(held?.from === openId ? held : null);
	$effect(() => {
		if (held && held.from !== openId) held = null;
	});
	/** What the album sheet shows. */
	let sheetAlbum = $derived(holding?.album ?? openAlbum);
	let sheetDetail = $derived(
		holding ? holding.detail : detail?.id === sheetAlbum?.id ? detail : null
	);

	/** Bumped by each load or save of `detail`, so an older answer can't land after a newer one. */
	let detailGen = 0;

	// Load the open album's tracks (again after signing back in, if that failed).
	$effect(() => {
		const id = openId;
		void session.generation;
		if (!id || untrack(() => detail?.id) === id) return;
		let cancelled = false;
		const gen = ++detailGen;
		const current = () => !cancelled && gen === detailGen;
		api
			.album(id)
			.then((d) => current() && (detail = d))
			.catch(() => current() && toast('Could not load album', 'error'));
		return () => (cancelled = true);
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
			target.style.viewTransitionName = 'album-cover';
		});
		heroCover?.style.setProperty('view-transition-name', 'album-cover');
		follow(vt, () => target);
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

	async function onsaved(result: TagWriteResult) {
		const from = openId;
		savedAs = result.album_id;
		// Saved over whatever changed elsewhere: the sheet shows the album as
		// saved straight away, so edits begun after Save stay.
		held = null;
		detailGen++;
		if (result.album) detail = result.album;
		if (!from || result.album_id === from) return loadAlbums();
		// A new title: the sheet stays while the grids catch up with the new id.
		if (sheetAlbum)
			held = {
				from,
				next: result.album_id,
				album: sheetAlbum,
				detail: result.album,
				changed: false
			};
		await loadAlbums();
		if (openId === from)
			await goto(page.url.href, {
				state: { album: result.album_id },
				shallow: true,
				replace: true
			});
	}

	let lastAlbums = 0;
	async function fetchAlbums() {
		try {
			const [inbox, managed] = await Promise.all([api.albums('inbox'), api.albums('library')]);
			// The open album has left the grids (renamed, imported or removed
			// elsewhere) while its tags have unsaved edits: the sheet stays on it.
			const gone = (id: string) =>
				!inbox.some((a) => a.id === id) && !managed.some((a) => a.id === id);
			// During a save it stays without a word: the save may be what moved it.
			if (openId && draft !== 'none' && !holding && openId !== importingHere && gone(openId))
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
		inboxSetup.load();
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
	/** The scan's progress as last pushed, which a poll answered before it mustn't undo. */
	let pushedScan: Status['scan'] | null = null;
	let pollTimer: ReturnType<typeof setTimeout>;
	async function poll() {
		clearTimeout(pollTimer);
		// Signed out: signing back in polls again.
		if (session.state === 'out') return;
		try {
			const wasRunning = status?.scan.running ?? false;
			const pushed = pushedScan;
			const fresh = await api.status();
			status = pushedScan && pushedScan !== pushed ? { ...fresh, scan: pushedScan } : fresh;
			if (wasRunning && !status.scan.running) await scanFinished();
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
	}

	/** The open album's id as of the latest notice (a held sheet's album may have a new one). */
	const openNow = () => holding?.next ?? openId;

	/** The open album: `null` if it's gone (404), `undefined` if that couldn't be told. */
	const fetchAlbum = (id: string) =>
		api.album(id).catch((e) => (e instanceof ApiError && e.status === 404 ? null : undefined));

	/** The open album's id from a notice that came during a save, checked once it's answered. */
	let duringSave: string | null = null;
	/** The id the last save that worked left the album with. */
	let savedAs: string | null = null;

	/**
	 * Keep the sheet on the open album as it shows it now, with its unsaved
	 * edits, and say it changed elsewhere (unless not `changed`, yet). `next` is
	 * the album's id now, if known.
	 */
	function hold(next: string | null, changed = true) {
		if (!openId || !sheetAlbum) return;
		// Older loads of the album, still under way, mustn't land over this.
		detailGen++;
		held = {
			from: openId,
			next: next ?? holding?.next ?? null,
			album: sheetAlbum,
			detail: sheetDetail,
			changed
		};
	}

	function ondraft(next: Draft) {
		const was = draft;
		draft = next;
		if (was !== 'saving' || next === 'saving') return;
		// The save is answered.
		const to = duringSave;
		const saved = savedAs;
		duringSave = savedAs = null;
		// It failed, and the album moved meanwhile (the sheet stayed for it): say so now.
		if (next === 'draft' && holding && !holding.changed) hold(holding.next);
		// A notice came while it ran. Changes made before the save are in its
		// answer; one made since is checked against what the sheet shows now.
		if (!to || (saved && to !== saved)) return;
		if (next !== 'draft') followOpen(to, Promise.resolve());
		else if (to !== openId) hold(to);
		else holdIfChanged(to);
	}

	/**
	 * Show the open album as it is now, under its new id `to` if it has one,
	 * once its tracks and the grids (`grids`) have both caught up. The sheet
	 * stays as it is meanwhile, rather than closing while neither has the new
	 * id. Unless `discard`, edits begun meanwhile hold the sheet instead (if
	 * the album changed), and a save begun meanwhile checks it once answered.
	 * Returns `gone` for a 404, and `failed` if the album couldn't be asked
	 * for (the daemon away, signed out): the sheet then stays, with Reload.
	 */
	async function followOpen(
		to: string,
		grids: Promise<void>,
		discard = false
	): Promise<'ok' | 'gone' | 'failed'> {
		const from = openId;
		if (!from || !sheetAlbum) return 'ok';
		// Where a follow under way was taking the sheet.
		const via = holding?.next;
		const gen = ++detailGen;
		if (to !== from)
			held = { from, next: to, album: sheetAlbum, detail: sheetDetail, changed: false };
		const [fresh] = await Promise.all([fetchAlbum(to), grids]);
		// The user opened another album, or a newer load, hold or save took over.
		if ((openId !== from && openId !== via) || gen !== detailGen) return 'ok';
		if (!discard && draft === 'saving') {
			duringSave = to;
			return 'ok';
		}
		if (fresh === undefined) {
			hold(to);
			return 'failed';
		}
		const shown = $state.snapshot(sheetDetail);
		// Unchanged (this page's own save, come back as a notice): the editor,
		// and any edits begun in it since, are left be.
		if (!discard && fresh && to === openId && sameJson(fresh, shown)) {
			held = null;
			return 'ok';
		}
		if (!discard && draft === 'draft') {
			hold(to);
			return 'ok';
		}
		if (!fresh || to === from) held = null;
		if (!fresh) return 'gone';
		detail = fresh;
		if (openId !== to)
			await goto(page.url.href, { state: { album: to }, shallow: true, replace: true });
		return 'ok';
	}

	/**
	 * A sheet with unsaved edits stays as it is, but says so if its album
	 * changed: after missed notices, or a notice that may be this page's own
	 * save come back. A later load or save makes the check moot.
	 */
	async function holdIfChanged(id: string) {
		const from = openId;
		const shown = $state.snapshot(sheetDetail);
		if (!shown) return;
		const gen = ++detailGen;
		const fresh = await fetchAlbum(id);
		if (gen !== detailGen || openId !== from || draft !== 'draft' || holding?.changed) return;
		// Not known (the daemon away, signed out): Reload is offered all the same.
		if (!fresh || !sameJson(fresh, shown)) hold(fresh === null ? null : id);
	}

	/** The open album changed elsewhere and is now `to`: follow it, unless that would lose edits. */
	function openChanged(to: string, grids: Promise<void>) {
		// A Reload under way goes on, to the album as it is now.
		if (reloading) followOpen(to, grids, true);
		// During a save: checked once it's answered (see ondraft).
		else if (draft === 'saving') duringSave = to;
		else if (draft === 'none') followOpen(to, grids);
		// With unsaved edits: a new id is a change; the same one may not be.
		else if (to !== openId) hold(to);
		else holdIfChanged(to);
	}

	/** Reload has been pressed, and the album is being fetched. */
	let reloading = false;

	/** Drop the held sheet's unsaved edits and show its album as it is now. */
	async function reloadHeld() {
		const h = holding;
		if (!h) return;
		reloading = true;
		const result = await followOpen(h.next ?? h.from, loadAlbums(), true).finally(
			() => (reloading = false)
		);
		if (result === 'failed') toast('Couldn’t reload the album. Try again in a moment.', 'error');
		if (result !== 'gone') return;
		// Its new id isn't known when the notice was missed (a hidden tab, the stream down).
		toast('This album was renamed, imported or removed elsewhere', 'info');
		await close();
	}

	/** An album was edited, here or elsewhere: show it as it is now. */
	function albumChanged({ id, previous_id, collection }: Notices['album_changed']) {
		// The Inbox's tag coverage, which Library setup shows, may have changed.
		if (collection === 'inbox') dataGeneration++;
		const grids = loadAlbums();
		const open = openNow();
		if (open !== null && (open === id || open === previous_id)) openChanged(id, grids);
	}

	/** Reload everything: notices were missed, or the stream has just come back. */
	function resync() {
		poll();
		appearance.refresh();
		syncSetup.load();
		setup.refresh();
		inboxSetup.load();
		dataGeneration++;
		const grids = loadAlbums();
		const open = openNow();
		if (!open) return;
		if (reloading) followOpen(open, grids, true);
		else if (draft === 'saving') duringSave = open;
		else if (draft === 'none') followOpen(open, grids);
		// Unless its album is gone (see fetchAlbums), or changed, or the
		// sheet is already held or on its way to a new id:
		else if (!holding) holdIfChanged(open);
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
			if (status) status = { ...status, scan };
			scanFinished();
		},
		imported({ album_id, inbox_id }) {
			// Copies hide the Inbox files, which Inbox setup counts.
			inboxSetup.load();
			const grids = inboxChanged();
			const open = openNow();
			// An import made here closes its sheet itself.
			if (open === inbox_id && inbox_id !== importingHere) openChanged(album_id, grids);
			// Tracks added to a Library album that's open.
			else if (open === album_id) openChanged(album_id, grids);
		},
		album_changed: albumChanged,
		setup_saved({ setup: saved }) {
			// Each can change which albums need attention, or what sorts and filters show.
			if (saved === 'library') setup.refresh();
			else if (saved === 'inbox') {
				// Hidden files may be back in the Inbox.
				inboxSetup.load();
				dataGeneration++;
			} else if (saved === 'appearance') appearance.refresh();
			else return syncSetup.load();
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
		inboxSetup.load();
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
	 * What the page needs from the daemon. Setups that have loaded aren't asked
	 * again (they may hold unsaved edits).
	 */
	function loadAll() {
		const reads: Promise<unknown>[] = [poll()];
		if (!appearance.loaded) reads.push(appearance.load());
		// Whether the sidebar shows its Sync section.
		if (!syncSetup.loaded) reads.push(syncSetup.load());
		// Whether an Inbox folder is set, for the empty Inbox's message.
		if (!inboxSetup.loaded) reads.push(inboxSetup.load());
		// Tag labels for attention badges (loaded once).
		reads.push(setup.load(), loadAlbums());
		return Promise.all(reads);
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
	   backdrop-filter on the sheet: Firefox leaves backdrop-filter out of
	   view-transition snapshots, so the blur dropped out while the cover moved. */
	.page.behind {
		filter: blur(18px) saturate(1.2);
	}
	/* Captured with the page while it's blurred: on its own it would lose the blur. */
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
