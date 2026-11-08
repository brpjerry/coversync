<script lang="ts">
	import { correction, generate } from 'lean-qr';
	import { toSvgPath } from 'lean-qr/extras/svg';

	interface Props {
		/** What the code holds. */
		text: string;
		/** What it's for, for screen readers. */
		label: string;
	}
	let { text, label }: Props = $props();

	// Medium error correction: phone screens and cameras at an angle read it
	// fine, and the code stays small enough to scan from across a desk.
	let code = $derived(generate(text, { minCorrectionLevel: correction.M }));
	/** The quiet zone scanners need around it, in modules. */
	const PAD = 4;
</script>

<svg
	class="qr"
	viewBox="{-PAD} {-PAD} {code.size + 2 * PAD} {code.size + 2 * PAD}"
	role="img"
	aria-label={label}
	shape-rendering="crispEdges"
>
	<rect x={-PAD} y={-PAD} width={code.size + 2 * PAD} height={code.size + 2 * PAD} />
	<path d={toSvgPath(code)} />
</svg>

<style>
	/* Dark on light in both themes: scanners expect it. */
	.qr {
		display: block;
		width: 100%;
		height: auto;
		border-radius: 14px;
	}
	rect {
		fill: #fff;
	}
	path {
		fill: #0a0b0e;
	}
</style>
