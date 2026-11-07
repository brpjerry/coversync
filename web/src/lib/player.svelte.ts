// Playback state shared by the player bar and album views.

import { coverUrl, streamUrl, type Album, type Track } from './api';

class Player {
	queue = $state<Track[]>([]);
	index = $state(-1);
	album = $state<Album | null>(null);
	playing = $state(false);
	buffering = $state(false);
	time = $state(0);
	duration = $state(0);
	volume = $state(0.85);
	muted = $state(false);

	current = $derived(this.queue[this.index] ?? null);

	#audio: HTMLAudioElement | null = null;
	#frame = 0;

	#el(): HTMLAudioElement {
		if (this.#audio) return this.#audio;
		const a = new Audio();
		a.preload = 'auto';
		a.volume = this.volume;
		a.addEventListener('play', () => {
			this.playing = true;
			this.#tick();
		});
		a.addEventListener('pause', () => (this.playing = false));
		// Buffering is read from the element on every event that can change it, rather than
		// set on `waiting` and cleared on `playing`: WebKitGTK fires no `playing` after a seek,
		// which left the spinner up, even once paused. Time moving on in normal playback
		// (not a seek) also means it isn't buffering, whatever readyState says.
		let last = 0;
		const buffering = (event: Event) => {
			const advancing = event.type === 'timeupdate' && !a.seeking && a.currentTime !== last;
			last = a.currentTime;
			this.buffering =
				!a.paused && !a.ended && !advancing && a.readyState < HTMLMediaElement.HAVE_FUTURE_DATA;
		};
		for (const event of [
			'waiting',
			'playing',
			'play',
			'pause',
			'seeking',
			'seeked',
			'canplay',
			'timeupdate',
			'emptied',
			'ended'
		]) {
			a.addEventListener(event, buffering);
		}
		a.addEventListener('loadedmetadata', () => (this.duration = a.duration || 0));
		a.addEventListener('ended', () => this.next());
		this.#audio = a;
		this.#mediaSession();
		return a;
	}

	// Poll currentTime every frame while playing; timeupdate is too coarse for a smooth bar.
	#tick = () => {
		cancelAnimationFrame(this.#frame);
		const step = () => {
			if (!this.#audio) return;
			this.time = this.#audio.currentTime;
			if (!this.#audio.paused) this.#frame = requestAnimationFrame(step);
		};
		step();
	};

	#load(autoplay = true) {
		const track = this.current;
		if (!track) return;
		const a = this.#el();
		a.src = streamUrl(track.id);
		this.time = 0;
		this.duration = track.duration_ms / 1000;
		if (autoplay) a.play().catch(() => (this.playing = false));
		this.#updateMetadata();
	}

	playAlbum(album: Album, tracks: Track[], start = 0) {
		this.album = album;
		this.queue = tracks;
		this.index = start;
		this.#load();
	}

	isCurrent(track: Track) {
		return this.current?.id === track.id;
	}

	toggle() {
		const a = this.#audio;
		if (!a || !this.current) return;
		if (a.paused) a.play();
		else a.pause();
	}

	next() {
		if (this.index < this.queue.length - 1) {
			this.index++;
			this.#load();
		} else {
			this.#audio?.pause();
			this.seek(0);
		}
	}

	prev() {
		if (this.time > 3 || this.index === 0) {
			this.seek(0);
		} else {
			this.index--;
			this.#load();
		}
	}

	seek(seconds: number) {
		if (!this.#audio) return;
		this.#audio.currentTime = Math.max(0, Math.min(seconds, this.duration || seconds));
		this.time = this.#audio.currentTime;
	}

	setVolume(v: number) {
		this.volume = Math.max(0, Math.min(1, v));
		this.muted = this.volume === 0;
		if (this.#audio) {
			this.#audio.volume = this.volume;
			this.#audio.muted = this.muted;
		}
	}

	toggleMute() {
		this.muted = !this.muted;
		if (this.#audio) this.#audio.muted = this.muted;
	}

	#updateMetadata() {
		const t = this.current;
		if (!t || !('mediaSession' in navigator)) return;
		const art = this.album?.has_cover
			? [
					{
						src: new URL(coverUrl(this.album, 512), location.href).href,
						sizes: '512x512',
						type: 'image/jpeg'
					}
				]
			: [];
		navigator.mediaSession.metadata = new MediaMetadata({
			title: t.title,
			artist: t.artist,
			album: t.album,
			artwork: art
		});
	}

	#mediaSession() {
		if (!('mediaSession' in navigator)) return;
		const ms = navigator.mediaSession;
		ms.setActionHandler('play', () => this.toggle());
		ms.setActionHandler('pause', () => this.toggle());
		ms.setActionHandler('nexttrack', () => this.next());
		ms.setActionHandler('previoustrack', () => this.prev());
		ms.setActionHandler('seekto', (d) => d.seekTime != null && this.seek(d.seekTime));
	}
}

export const player = new Player();
