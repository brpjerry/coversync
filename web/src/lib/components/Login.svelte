<script lang="ts">
	import { fade, fly } from 'svelte/transition';
	import { cubicOut } from 'svelte/easing';
	import LockKeyhole from '@lucide/svelte/icons/lock-keyhole';
	import CircleAlert from '@lucide/svelte/icons/circle-alert';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';
	import LogIn from '@lucide/svelte/icons/log-in';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import { session } from '#lib/session.svelte.ts';

	let password = $state('');
	let busy = $state(false);
	let problem = $state<string | null>(null);
	let input = $state<HTMLInputElement>();
	/** Over the app after its session ended: the app stays, blurred, underneath. */
	let over = $derived(session.shown);

	let title = $derived(
		session.why === 'no_password'
			? 'Set a password first'
			: over
				? 'You’ve been signed out'
				: session.why === 'session_expired'
					? 'Your session has ended'
					: 'Sign in'
	);

	async function submit(e: SubmitEvent) {
		e.preventDefault();
		if (!password || busy) return;
		busy = true;
		problem = await session.login(password);
		busy = false;
		if (problem) input?.select();
		else password = '';
	}

	async function recheck() {
		busy = true;
		await session.check();
		busy = false;
	}

	// Another tab may have signed in meanwhile.
	function onfocus() {
		if (session.state === 'out' && session.why !== 'no_password' && !busy) session.check();
	}
</script>

<svelte:window {onfocus} />

<div class="login" class:over transition:fade={{ duration: 200 }}>
	<div
		class="card"
		role="dialog"
		aria-modal="true"
		aria-labelledby="login-title"
		in:fly={{ y: 14, duration: 460, easing: cubicOut }}
	>
		<div class="brand"><span class="logo" aria-hidden="true"></span>coversync</div>
		<h1 id="login-title">{title}</h1>
		{#if session.why === 'no_password'}
			<p>This server has no password yet, so nothing can be opened. On the server, run:</p>
			<pre><code>coversyncd set-password</code></pre>
			<p class="note">
				With the same <code>--state-dir</code> as the service, if it has one. Then sign in here.
			</p>
			<button class="btn primary wide" onclick={recheck} disabled={busy}>
				{#if busy}<span class="spin"><LoaderCircle size={16} /></span>{:else}<RefreshCw
						size={16}
					/>{/if}
				Check again
			</button>
		{:else}
			<p>
				{over
					? 'Sign in again to carry on. Unsaved changes are kept.'
					: session.why === 'session_expired'
						? 'Sign in again to carry on.'
						: 'Enter the password set on the server.'}
			</p>
			<form onsubmit={submit}>
				<!-- For password managers: there's one user. -->
				<input type="text" name="username" autocomplete="username" value="coversync" hidden />
				<label class="field" class:invalid={!!problem}>
					<span class="lead" aria-hidden="true"><LockKeyhole size={16} /></span>
					<span class="sr-only">Password</span>
					<input
						bind:this={input}
						bind:value={password}
						oninput={() => (problem = null)}
						type="password"
						name="password"
						autocomplete="current-password"
						placeholder="Password"
						aria-invalid={!!problem}
						aria-describedby={problem ? 'login-problem' : undefined}
						{@attach (el: HTMLInputElement) => el.focus()}
					/>
				</label>
				{#if problem}
					<p class="problem" id="login-problem" role="alert" transition:fade={{ duration: 150 }}>
						<CircleAlert size={14} strokeWidth={2.2} />
						{problem}
					</p>
				{/if}
				<button class="btn primary wide" type="submit" disabled={busy || !password}>
					{#if busy}<span class="spin"><LoaderCircle size={16} /></span>{:else}<LogIn
							size={16}
						/>{/if}
					Sign in
				</button>
			</form>
			<p class="note">Forgot it? Run <code>coversyncd set-password</code> on the server.</p>
		{/if}
	</div>
</div>

<style>
	.login {
		position: fixed;
		inset: 0;
		z-index: 100;
		display: grid;
		place-items: center;
		padding: 24px 16px;
		overflow-y: auto;
		background:
			radial-gradient(
				60rem 36rem at 50% -10%,
				color-mix(in oklab, var(--accent) 16%, transparent),
				transparent 70%
			),
			var(--bg);
	}
	/* Over the app: its page shows through, frosted. */
	.login.over {
		background: var(--bg-overlay);
		backdrop-filter: blur(22px) saturate(1.2);
	}
	.card {
		width: min(100%, 380px);
		padding: 30px 28px 24px;
		border-radius: 22px;
		background: var(--bg-raised);
		border: 1px solid var(--border);
		box-shadow: var(--shadow);
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-bottom: 22px;
		font-weight: 680;
		font-size: 17px;
		letter-spacing: -0.02em;
	}
	/* The top bar's mark. */
	.logo {
		width: 26px;
		height: 26px;
		border-radius: 8px;
		background:
			radial-gradient(circle at 50% 50%, var(--bg-raised) 0 3.5px, transparent 4px),
			conic-gradient(
				from 200deg,
				hsl(252 85% 74%),
				hsl(320 80% 70%),
				hsl(30 90% 66%),
				hsl(252 85% 74%)
			);
		box-shadow: 0 4px 14px -4px hsl(280 80% 60% / 0.6);
	}
	h1 {
		margin: 0 0 6px;
		font-size: 22px;
		letter-spacing: -0.02em;
	}
	p {
		margin: 0 0 18px;
		color: var(--text-2);
		font-size: 14px;
	}
	.note {
		margin: 16px 0 0;
		font-size: 12.5px;
		color: var(--text-3);
	}
	code {
		font-size: 0.92em;
		color: var(--text);
	}
	pre {
		margin: 0 0 4px;
		padding: 12px 14px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--border);
		overflow-x: auto;
		font-size: 14px;
	}
	form {
		display: grid;
		gap: 12px;
	}
	.field {
		position: relative;
		display: flex;
		align-items: center;
		height: 44px;
		border-radius: 12px;
		background: var(--surface);
		border: 1px solid var(--border);
		color: var(--text-3);
		transition:
			border-color 160ms,
			background 160ms,
			box-shadow 200ms;
	}
	.field:hover {
		border-color: var(--border-strong);
	}
	.field:focus-within {
		border-color: color-mix(in oklab, var(--accent) 60%, transparent);
		background: var(--bg-raised);
		box-shadow: 0 0 0 4px color-mix(in oklab, var(--accent) 16%, transparent);
	}
	.field.invalid {
		border-color: color-mix(in oklab, var(--danger) 70%, transparent);
	}
	.lead {
		position: absolute;
		left: 14px;
		display: grid;
		pointer-events: none;
	}
	.field input {
		width: 100%;
		height: 100%;
		min-width: 0;
		padding: 0 14px 0 40px;
		border: 0;
		outline: 0;
		background: none;
		font-size: 15px;
		color: var(--text);
	}
	.field input::placeholder {
		color: var(--text-3);
	}
	.problem {
		display: flex;
		align-items: center;
		gap: 6px;
		margin: -2px 0 0;
		font-size: 13px;
		color: var(--danger);
	}
	.problem :global(svg) {
		flex: none;
	}
	.wide {
		justify-content: center;
		width: 100%;
		height: 42px;
	}
	.card > .wide {
		margin-top: 20px;
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
