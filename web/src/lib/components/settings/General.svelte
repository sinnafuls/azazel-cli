<script lang="ts">
	// What this browser keeps for itself: the theme and its accent, the text size, what Enter does,
	// whether thinking is shown, and notifications.
	import Segmented from '$lib/components/Segmented.svelte';
	import { Switch } from '$lib/components/ui/switch';
	import * as NativeSelect from '$lib/components/ui/native-select';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import * as ToggleGroup from '$lib/components/ui/toggle-group';
	import * as Field from '$lib/components/ui/field';
	import { app, THEMES, type EnterKey, type TextSize, type ThemeChoice } from '$lib/app.svelte';
	import { ACCENTS, accentColor, mapTheme, preview, type Accent, type Preview } from '$lib/theme';
	import { toasts } from '$lib/toasts.svelte';
	import Section from './Section.svelte';
	import Row from './Row.svelte';

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
	// Each theme card draws the page small, in that theme's own colours and the accent in use.
	// "Match terminal" is drawn as the terminal's theme once a session has reported it.
	const terminal = $derived(app.terminal ? mapTheme(app.terminal) : null);
	const pictures = $derived<Record<ThemeChoice, Preview>>({
		terminal: preview(false, app.accent, terminal),
		dark: preview(false, app.accent),
		light: preview(true, app.accent)
	});
	// A matched terminal theme brings its own accent, so the choice waits for the dark or light theme.
	const ownAccent = $derived(app.theme === 'terminal' && !!app.mapped);

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

<Section title="Theme" id="set-theme">
	<RadioGroup.Root
		aria-labelledby="set-theme"
		class="sm:grid-cols-3"
		value={app.theme}
		onValueChange={(v) => app.setTheme(v as (typeof THEMES)[number]['value'])}
	>
		{#each THEMES as t (t.value)}
			{@const c = pictures[t.value]}
			<Field.Label for="set-theme-{t.value}" data-theme-card={t.value}>
				<!-- The page in small: the sidebar with a chosen row, two lines of a reply, the message box. -->
				<span
					class="m-2.5 mb-0 flex h-16 self-stretch overflow-hidden rounded-md border"
					style="background: {c.background}; border-color: {c.border}"
					aria-hidden="true"
					data-theme-preview
				>
					<span
						class="flex w-1/4 flex-col gap-1 border-r p-1.5"
						style="background: {c.sidebar}; border-color: {c.border}"
					>
						<span class="h-1.5 rounded-full" style="background: {c['sidebar-accent']}"></span>
						<span class="h-1 w-2/3 rounded-full" style="background: {c['muted-foreground']}"></span>
						<span class="h-1 w-1/2 rounded-full" style="background: {c['muted-foreground']}"></span>
					</span>
					<span class="flex min-w-0 flex-1 flex-col gap-1 p-1.5">
						<span class="h-1 w-3/4 rounded-full" style="background: {c.foreground}"></span>
						<span class="h-1 w-1/2 rounded-full" style="background: {c['muted-foreground']}"></span>
						<span
							class="mt-auto flex h-4 items-center justify-end rounded-sm border px-1"
							style="background: {c.card}; border-color: {c.border}"
						>
							<span class="size-2 rounded-full" style="background: {c.brand}" data-preview-accent
							></span>
						</span>
					</span>
				</span>
				<Field.Field orientation="horizontal">
					<RadioGroup.Item value={t.value} id="set-theme-{t.value}" />
					<Field.Content>
						<Field.Title>{t.label}</Field.Title>
						<Field.Description class="text-xs">{t.hint}</Field.Description>
					</Field.Content>
				</Field.Field>
			</Field.Label>
		{/each}
	</RadioGroup.Root>
	{#if app.theme === 'terminal'}
		<p class="mt-2 text-xs leading-relaxed text-muted-foreground" data-terminal-theme>
			{#if app.terminal}
				Following <span class="font-mono text-foreground"
					>{app.terminal.name || 'the terminal'}</span
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
</Section>

<Section title="Accent">
	<Row
		label="Accent colour"
		hint={ownAccent
			? "Match terminal uses the terminal theme's own accent."
			: 'Focus, what is running or chosen, and the meters.'}
		id="set-accent"
		orientation="responsive"
	>
		<ToggleGroup.Root
			type="single"
			variant="outline"
			spacing={1}
			aria-labelledby="set-accent"
			disabled={ownAccent}
			data-accents
			bind:value={
				() => app.accent,
				(v) => {
					if (v && v !== app.accent) app.setAccent(v as Accent);
				}
			}
		>
			{#each ACCENTS as a (a.value)}
				<ToggleGroup.Item
					value={a.value}
					aria-label={a.label}
					title={a.label}
					class="px-0 data-[state=on]:border-ring"
				>
					<span
						class="size-4 rounded-full"
						style="background: {accentColor(a.value, app.light)}"
						data-swatch={a.value}
					></span>
				</ToggleGroup.Item>
			{/each}
		</ToggleGroup.Root>
	</Row>
</Section>

<Section title="Reading">
	<Row label="Text size" hint="Scales the whole page." id="set-size" orientation="responsive">
		<Segmented
			class="w-56 shrink-0"
			labelledby="set-size"
			value={app.textSize}
			onchange={(v) => app.setTextSize(v)}
			options={SIZES.map(([value, label]) => ({ value, label }))}
		/>
	</Row>
</Section>

<Section title="Message box">
	<Row label="Enter" for="set-enter" orientation="responsive">
		<NativeSelect.Root
			id="set-enter"
			value={app.enter}
			onchange={(e) => app.setEnter(e.currentTarget.value as EnterKey)}
		>
			{#each ENTER as [v, label] (v)}
				<NativeSelect.Option value={v}>{label}</NativeSelect.Option>
			{/each}
		</NativeSelect.Root>
	</Row>
</Section>

<Section title="Transcript">
	<Row
		label="Thinking"
		hint="Follow terminal uses the terminal's tui.show_thinking setting."
		id="set-thinking"
		orientation="responsive"
	>
		<Segmented
			class="w-72 max-w-full shrink-0"
			labelledby="set-thinking"
			value={app.thinking}
			onchange={(v) => app.setThinking(v)}
			options={[
				{ value: 'follow', label: 'Follow terminal' },
				{ value: 'show', label: 'Show' },
				{ value: 'hide', label: 'Hide' }
			]}
		/>
	</Row>
</Section>

<Section title="Notifications">
	<Row
		label="Notify when a session needs you"
		hint="A browser notification while this page is in the background. It stays on this device."
		id="set-notify"
	>
		<Switch
			aria-labelledby="set-notify"
			checked={app.notify}
			disabled={!canNotify}
			onCheckedChange={(v) => notify(v)}
		/>
	</Row>
</Section>
