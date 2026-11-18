<script lang="ts">
	import { app, THEMES, type EnterKey, type TextSize } from '$lib/app.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import Check from '@lucide/svelte/icons/check';

	const SIZES: [TextSize, string][] = [
		['small', 'Small'],
		['default', 'Default'],
		['large', 'Large']
	];
	const ENTER: [EnterKey, string][] = [
		['auto', 'Sends with a keyboard, new line on touch'],
		['send', 'Always sends'],
		['newline', 'Always makes a new line']
	];
	const canNotify = typeof Notification !== 'undefined';
	const fellBack = $derived(app.mapped?.fellBack.length ?? 0);

	async function notify(on: boolean) {
		if (!(await app.setNotify(on)) && on)
			toasts.push(
				canNotify
					? 'The browser blocks notifications for this page. Allow them in its site settings.'
					: 'This browser has no notifications.',
				'warn'
			);
	}
</script>

<h3 class="set-h" id="set-theme">Theme</h3>
<div role="radiogroup" aria-labelledby="set-theme" class="grid gap-2 sm:grid-cols-3">
	{#each THEMES as t (t.value)}
		<button
			type="button"
			role="radio"
			aria-checked={app.theme === t.value}
			class="choice flex flex-col gap-2 p-2.5 text-left"
			onclick={() => app.setTheme(t.value)}
		>
			<span class="swatch swatch-{t.value}" aria-hidden="true">
				<span></span><span></span><span></span>
			</span>
			<span class="flex items-center gap-1.5 text-[13px] font-medium"
				>{t.label}{#if app.theme === t.value}<Check class="size-3.5" />{/if}</span
			>
			<span class="text-muted-foreground text-xs leading-snug">{t.hint}</span>
		</button>
	{/each}
</div>
{#if app.theme === 'terminal'}
	<p class="text-muted-foreground mt-2 text-xs leading-relaxed" data-terminal-theme>
		{#if app.terminal}
			Following <span class="text-foreground font-mono">{app.terminal.name || 'the terminal'}</span
			>{app.terminal.light ? ' (light)' : ''}.
			{#if fellBack}
				{fellBack} colour{fellBack === 1 ? '' : 's'} kept the built-in value: the terminal's
				{fellBack === 1 ? 'was' : 'were'} too faint to read here.
			{/if}
		{:else}
			The terminal's theme arrives when a session is open. Until then this is the dark theme.
		{/if}
	</p>
{/if}

<h3 class="set-h">Reading</h3>
<div class="set-row">
	<div>
		<p class="set-label" id="set-size">Text size</p>
		<p class="set-hint">Scales the whole page.</p>
	</div>
	<div class="seg w-56 shrink-0" role="radiogroup" aria-labelledby="set-size">
		{#each SIZES as [v, label] (v)}
			<button
				type="button"
				role="radio"
				aria-checked={app.textSize === v}
				onclick={() => app.setTextSize(v)}>{label}</button
			>
		{/each}
	</div>
</div>

<h3 class="set-h">Message box</h3>
<div class="set-row">
	<label class="set-label" for="set-enter">Enter</label>
	<select
		id="set-enter"
		class="select"
		value={app.enter}
		onchange={(e) => app.setEnter(e.currentTarget.value as EnterKey)}
	>
		{#each ENTER as [v, label] (v)}<option value={v}>{label}</option>{/each}
	</select>
</div>

<h3 class="set-h">Notifications</h3>
<div class="set-row">
	<div>
		<p class="set-label" id="set-notify">Notify when a session needs you</p>
		<p class="set-hint">
			A browser notification while this page is in the background. It stays on this device.
		</p>
	</div>
	<button
		type="button"
		class="switch"
		role="switch"
		aria-checked={app.notify}
		aria-labelledby="set-notify"
		disabled={!canNotify}
		onclick={() => notify(!app.notify)}
	></button>
</div>

<style>
	/* A thumbnail of a theme: a rail, a canvas and a line of ink. The colours are fixed, not tokens. */
	.swatch {
		position: relative;
		display: block;
		height: 3.25rem;
		overflow: hidden;
		border-radius: var(--radius-sm);
		box-shadow: inset 0 0 0 1px var(--border-strong);
		background: var(--sw-bg);
	}
	.swatch span:nth-child(1) {
		position: absolute;
		inset: 0 auto 0 0;
		width: 26%;
		background: var(--sw-rail);
	}
	.swatch span:nth-child(2) {
		position: absolute;
		top: 34%;
		left: 36%;
		width: 44%;
		height: 4px;
		border-radius: 2px;
		background: var(--sw-ink);
	}
	.swatch span:nth-child(3) {
		position: absolute;
		top: 56%;
		left: 36%;
		width: 28%;
		height: 4px;
		border-radius: 2px;
		background: var(--sw-ink);
		opacity: 0.5;
	}
	.swatch-dark {
		--sw-bg: #161616;
		--sw-rail: #0b0b0b;
		--sw-ink: #eeeeee;
	}
	.swatch-light {
		--sw-bg: #ffffff;
		--sw-rail: #f0f0f0;
		--sw-ink: #161616;
	}
	.swatch-terminal {
		--sw-bg: #1a1b26;
		--sw-rail: #13141c;
		--sw-ink: #7aa2f7;
	}
</style>
