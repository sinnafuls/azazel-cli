<script lang="ts">
	// The settings window: a Dialog holding vertical Tabs, the list of panes on the left and the open
	// pane beside it. On a phone it fills the screen and the list is its first page.
	import { tick, type Component } from 'svelte';
	import { ui, PANES, type Pane } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import * as Dialog from '$lib/components/ui/dialog';
	import * as Tabs from '$lib/components/ui/tabs';
	import { Button } from '$lib/components/ui/button';
	import General from './General.svelte';
	import Models from './Models.svelte';
	import Permissions from './Permissions.svelte';
	import UsagePane from './UsagePane.svelte';
	import ContextPane from './ContextPane.svelte';
	import AllSettings from './AllSettings.svelte';
	import Devices from './Devices.svelte';
	import Shortcuts from './Shortcuts.svelte';
	import About from './About.svelte';
	import X from '@lucide/svelte/icons/x';
	import ChevronLeft from '@lucide/svelte/icons/chevron-left';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import SlidersHorizontal from '@lucide/svelte/icons/sliders-horizontal';
	import Cpu from '@lucide/svelte/icons/cpu';
	import Shield from '@lucide/svelte/icons/shield';
	import Gauge from '@lucide/svelte/icons/gauge';
	import Layers from '@lucide/svelte/icons/layers';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Smartphone from '@lucide/svelte/icons/smartphone';
	import Keyboard from '@lucide/svelte/icons/keyboard';
	import Info from '@lucide/svelte/icons/info';

	const VIEW: Record<Pane, { icon: Component<{ class?: string }>; view: Component }> = {
		general: { icon: SlidersHorizontal, view: General },
		models: { icon: Cpu, view: Models },
		permissions: { icon: Shield, view: Permissions },
		usage: { icon: Gauge, view: UsagePane },
		context: { icon: Layers, view: ContextPane },
		all: { icon: Settings2, view: AllSettings },
		devices: { icon: Smartphone, view: Devices },
		shortcuts: { icon: Keyboard, view: Shortcuts },
		about: { icon: Info, view: About }
	};

	let body = $state<HTMLElement | null>(null);

	const open = $derived(ui.settings !== null);
	// Wide: a pane is always open. A phone: '' is the list.
	const pane = $derived<Pane | ''>(
		ui.settings === null ? '' : ui.settings === '' && viewport.wide ? 'general' : ui.settings
	);
	const title = $derived(PANES.find(([id]) => id === pane)?.[1] ?? 'Settings');
	const View = $derived(pane ? VIEW[pane].view : null);
	const showList = $derived(viewport.wide || pane === '');

	$effect(() => {
		void pane;
		body?.scrollTo({ top: 0 });
	});

	async function back() {
		const from = pane;
		ui.settings = '';
		await tick();
		document.getElementById(`set-tab-${from}`)?.focus();
	}
</script>

<Dialog.Root {open} onOpenChange={(v) => !v && (ui.settings = null)}>
	<Dialog.Content
		showCloseButton={false}
		data-settings
		class="flex h-dvh max-w-none gap-0 overflow-hidden rounded-none p-0 ring-0 sm:max-w-none md:h-[min(42rem,calc(100dvh-4rem))] md:max-w-[min(56rem,calc(100%-3rem))] md:rounded-xl md:ring-1"
	>
		<Dialog.Description class="sr-only">Settings of this page and of azazel</Dialog.Description>
		<!-- With the pane beside the list, moving through the list shows each pane; on a phone a tab
		     opens its page only when it is pressed. -->
		<Tabs.Root
			orientation="vertical"
			activationMode={viewport.wide ? 'automatic' : 'manual'}
			bind:value={() => pane, (v) => (ui.settings = v as Pane)}
			class="h-full min-h-0 w-full gap-0"
		>
			{#if showList}
				<div class="flex min-h-0 shrink-0 flex-col bg-sidebar max-md:w-full md:w-56 md:border-r">
					<div class="flex h-14 shrink-0 items-center justify-between px-4">
						<Dialog.Title class="leading-7">Settings</Dialog.Title>
						{#if !viewport.wide}
							<Button
								variant="ghost"
								size="icon-sm"
								class="-mr-1.5"
								aria-label="Close settings"
								onclick={() => (ui.settings = null)}><X /></Button
							>
						{/if}
					</div>
					<div class="scroll-thin min-h-0 flex-1 overflow-y-auto px-2 pb-2">
						<Tabs.List
							aria-label="Settings"
							class="w-full items-stretch justify-start gap-0.5 rounded-none bg-transparent p-0"
						>
							{#each PANES as [id, label] (id)}
								{@const Icon = VIEW[id].icon}
								<Tabs.Trigger
									value={id}
									id="set-tab-{id}"
									class="h-10 flex-none gap-2 px-2 font-normal md:h-8"
								>
									<Icon />
									<span class="min-w-0 flex-1 truncate text-left" data-row-text>{label}</span>
									{#if !viewport.wide}<ChevronRight class="opacity-60" />{/if}
								</Tabs.Trigger>
							{/each}
						</Tabs.List>
					</div>
				</div>
			{:else}
				<Dialog.Title class="sr-only">Settings</Dialog.Title>
			{/if}
			{#if pane && View}
				<div class="flex min-h-0 min-w-0 flex-1 flex-col">
					<div class="flex h-14 shrink-0 items-center gap-2 border-b px-4 md:px-6">
						{#if !viewport.wide}
							<Button
								variant="ghost"
								size="icon-sm"
								class="-ml-1.5"
								aria-label="All settings panes"
								onclick={back}><ChevronLeft /></Button
							>
						{/if}
						<h2 id="settings-title" class="min-w-0 flex-1 truncate text-sm leading-7 font-medium">
							{title}
						</h2>
						<Button
							variant="ghost"
							size="icon-sm"
							class="-mr-1.5"
							aria-label="Close settings"
							onclick={() => (ui.settings = null)}><X /></Button
						>
					</div>
					<Tabs.Content
						bind:ref={body}
						value={pane}
						class="scroll-thin min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-6"
						data-pane={pane}
					>
						{#key pane}<View />{/key}
					</Tabs.Content>
				</div>
			{/if}
		</Tabs.Root>
	</Dialog.Content>
</Dialog.Root>
