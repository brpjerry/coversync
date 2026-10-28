<script lang="ts">
	interface Props {
		checked: boolean;
		label: string;
		description?: string;
		disabled?: boolean;
		/** Indented under the option it depends on. */
		nested?: boolean;
	}
	let {
		checked = $bindable(),
		label,
		description,
		disabled = false,
		nested = false
	}: Props = $props();
</script>

<button
	class="toggle"
	class:nested
	role="switch"
	aria-checked={checked}
	{disabled}
	onclick={() => (checked = !checked)}
>
	<span class="text">
		<span class="label">{label}</span>
		{#if description}<span class="description">{description}</span>{/if}
	</span>
	<span class="switch" class:on={checked} aria-hidden="true"><span class="knob"></span></span>
</button>

<style>
	.toggle {
		display: flex;
		align-items: center;
		gap: 16px;
		width: 100%;
		padding: 12px 14px;
		border-radius: 12px;
		text-align: left;
		transition:
			background 140ms,
			opacity 200ms;
	}
	.toggle:hover:not(:disabled) {
		background: var(--surface);
	}
	.toggle:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.nested {
		padding-left: 38px;
	}
	.text {
		flex: 1;
		min-width: 0;
		display: grid;
		gap: 2px;
	}
	.label {
		font-size: 14px;
		font-weight: 540;
	}
	.description {
		font-size: 12.5px;
		color: var(--text-3);
	}
	.switch {
		flex: none;
		position: relative;
		width: 38px;
		height: 22px;
		border-radius: 999px;
		background: var(--border-strong);
		transition: background 200ms;
	}
	.switch.on {
		background: var(--accent);
	}
	.knob {
		position: absolute;
		top: 3px;
		left: 3px;
		width: 16px;
		height: 16px;
		border-radius: 999px;
		background: #fff;
		box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
		transition: transform 260ms var(--ease-spring);
	}
	.on .knob {
		transform: translateX(16px);
	}
</style>
