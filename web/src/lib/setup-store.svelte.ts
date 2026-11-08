// What every setup store (Library, Inbox, Sync, Appearance) shares: a first
// load, re-reads after saves made elsewhere, and its own saves, kept in order.
// Only the newest read lands, and nothing read before or during a save lands
// on top of it: such a read is asked again once the save is done, so it's
// never applied against the baseline the save replaced.

export abstract class SetupStore<R> {
	loaded = $state(false);
	failed = $state(false);
	saving = $state(false);

	/** Bumped by each read: only the newest lands. */
	#reads = 0;
	/** Bumped as each write starts and ends. */
	#writes = 0;
	/** Writes under way. */
	#writing = 0;
	/** A read was put off for a write: read again once writes are done. */
	#again = false;
	#loading: Promise<void> | null = null;

	/** Ask the daemon for the setup. */
	protected abstract fetch(): Promise<R>;
	/** Take an answer: the first one whole; later ones keeping unsaved edits. */
	protected abstract take(r: R, first: boolean): void;

	/**
	 * The first load. Once the setup has loaded, it returns at once; while it
	 * loads, it returns the same promise. After a failure (`failed`), calling
	 * it again tries again.
	 */
	load(): Promise<void> {
		if (this.loaded) return Promise.resolve();
		this.#loading ??= this.#read()
			.then(() => {
				// Asked again while it loaded: its answer may be from before that.
				if (this.#again && !this.#writing) this.#rerun();
			})
			.catch(() => {
				this.failed = !this.loaded;
			})
			.finally(() => (this.#loading = null));
		return this.#loading;
	}

	/**
	 * Read the saved setup again (after a save elsewhere, say), keeping
	 * unsaved edits; the first load if there hasn't been one.
	 */
	refresh(): Promise<void> {
		if (!this.loaded) {
			// A first load under way may have been answered before what this
			// was asked about: it reads again once it lands.
			if (this.#loading) this.#again = true;
			return this.load();
		}
		return this.#read().catch(() => {});
	}

	async #read() {
		const read = ++this.#reads;
		const writes = this.#writes;
		// The first answer has no baseline to be wrong about.
		if (this.loaded && this.#writing) {
			this.#again = true;
			return;
		}
		const r = await this.fetch();
		// A newer read lands instead.
		if (read !== this.#reads) return;
		if (this.loaded && (this.#writing || writes !== this.#writes)) {
			this.#again = true;
			if (!this.#writing) this.#rerun();
			return;
		}
		this.take(r, !this.loaded);
		this.loaded = true;
		this.failed = false;
	}

	#rerun() {
		this.#again = false;
		void this.refresh();
	}

	/**
	 * Run a write (a save): reads asked for meanwhile wait until it's done.
	 * `saving` shows it, unless `quiet`.
	 */
	protected async write<T>(send: () => Promise<T>, quiet = false): Promise<T> {
		this.#writing++;
		this.#writes++;
		if (!quiet) this.saving = true;
		try {
			return await send();
		} finally {
			this.#writing--;
			this.#writes++;
			if (!quiet) this.saving = false;
			if (!this.#writing && this.#again) this.#rerun();
		}
	}
}
