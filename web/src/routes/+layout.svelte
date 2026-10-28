<script lang="ts">
	import '@fontsource-variable/inter';
	import '../app.css';
	import favicon from '#lib/assets/favicon.svg';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<svelte:head>
	<title>coversync</title>
	<link rel="icon" href={favicon} />
</svelte:head>

{@render children()}
