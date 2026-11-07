<script lang="ts">
	import { onMount } from 'svelte';
	import '@fontsource-variable/inter';
	import '../app.css';
	import favicon from '#lib/assets/favicon.svg';
	import Login from '#lib/components/Login.svelte';
	import { session } from '#lib/session.svelte.ts';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	onMount(() => session.check());
</script>

<svelte:head>
	<title>coversync</title>
	<link rel="icon" href={favicon} />
</svelte:head>

<!-- The app starts once signed in, and stays (inert) if its session ends. -->
{#if session.shown}
	<div class="app" inert={session.state === 'out'}>
		{@render children()}
	</div>
{/if}
{#if session.state === 'out'}
	<Login />
{/if}

<style>
	.app {
		display: contents;
	}
</style>
