<script lang="ts">
	// The sidebar, on the shadcn Sidebar exactly as sin-api's admin shell builds its own: the brand,
	// New chat / Resume / Search, the sessions, and at the bottom what this device may do and the way
	// into Settings. Folded it is a rail of the same actions as icons; on a phone it is a sheet.
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { ui } from '$lib/ui.svelte';
	import { viewport } from '$lib/media.svelte';
	import { newChat } from '$lib/newchat.svelte';
	import { resume } from '$lib/resume.svelte';
	import { sessions } from '$lib/sessions.svelte';
	import { isMac, keysOf } from '$lib/shortcuts';
	import * as Sidebar from '$lib/components/ui/sidebar';
	import Brand from './Brand.svelte';
	import Tip from './Tip.svelte';
	import SessionList from './SessionList.svelte';
	import PanelLeft from '@lucide/svelte/icons/panel-left';
	import SquarePen from '@lucide/svelte/icons/square-pen';
	import Search from '@lucide/svelte/icons/search';
	import History from '@lucide/svelte/icons/history';
	import Settings from '@lucide/svelte/icons/settings';
	import ShieldAlert from '@lucide/svelte/icons/shield-alert';

	let { class: className }: { class?: string } = $props();

	const sidebar = Sidebar.useSidebar();
	ui.sidebar = sidebar;

	const mac = isMac(typeof navigator === 'undefined' ? '' : navigator.platform);
	const keys = (combo: string) => keysOf(combo, mac).join(mac ? '' : '+');
	const current = $derived(page.params.id ?? null);
	const waiting = $derived(sessions.list.filter((s) => s.state === 'waiting for you').length);
	const access = $derived(
		app.fullAccess ? 'Full access' : app.canControl ? 'Full control' : 'Watch only'
	);
	const folded = $derived(!sidebar.isMobile && sidebar.state === 'collapsed');
	const close = () => sidebar.setOpenMobile(false);
	const hint = 'font-normal text-muted-foreground pointer-coarse:hidden';
</script>

<Sidebar.Root collapsible="icon" class={className} aria-label="Sidebar">
	<Sidebar.Header class="h-14 flex-row items-center justify-between">
		<a
			href="/"
			class="rounded-md px-2 outline-none group-data-[collapsible=icon]:hidden focus-visible:ring-2 focus-visible:ring-sidebar-ring"
			aria-label="azazel, all sessions"
			onclick={close}><Brand /></a
		>
		<Tip
			text="{folded ? 'Show' : sidebar.isMobile ? 'Close' : 'Hide'} the sidebar ({keys(
				'mod+shift+s'
			)})"
			side={folded ? 'right' : 'bottom'}
			class="group-data-[collapsible=icon]:mx-auto"
			aria-label={folded
				? 'Show the sidebar'
				: sidebar.isMobile
					? 'Close the sidebar'
					: 'Hide the sidebar'}
			aria-expanded={!folded}
			onclick={() => sidebar.toggle()}><PanelLeft /></Tip
		>
	</Sidebar.Header>

	<Sidebar.Content class="scroll-thin">
		<Sidebar.Group>
			<Sidebar.GroupContent>
				<Sidebar.Menu>
					{#if newChat.available}
						<Sidebar.MenuItem>
							<Sidebar.MenuButton
								tooltipContent="New chat ({keys('mod+shift+o')})"
								aria-label="New chat"
								onclick={() => {
									close();
									newChat.show();
								}}
							>
								<SquarePen />
								<span>New chat</span>
							</Sidebar.MenuButton>
							<Sidebar.MenuBadge class={hint} aria-hidden="true"
								>{keys('mod+shift+o')}</Sidebar.MenuBadge
							>
						</Sidebar.MenuItem>
					{/if}
					{#if resume.available}
						<Sidebar.MenuItem>
							<Sidebar.MenuButton
								tooltipContent="Resume a session"
								aria-label="Resume a session"
								onclick={() => {
									close();
									resume.show();
								}}
							>
								<History />
								<span>Resume…</span>
							</Sidebar.MenuButton>
						</Sidebar.MenuItem>
					{/if}
					<Sidebar.MenuItem>
						<Sidebar.MenuButton
							tooltipContent="Search sessions ({keys('mod+k')})"
							aria-label={waiting
								? `Search sessions, ${waiting} waiting for you`
								: 'Search sessions'}
							onclick={() => {
								close();
								ui.search = true;
							}}
						>
							<Search />
							<span>Search</span>
						</Sidebar.MenuButton>
						<Sidebar.MenuBadge class={hint} aria-hidden="true">{keys('mod+k')}</Sidebar.MenuBadge>
						{#if waiting}
							<!-- On the rail the list is folded away: this mark says a session waits for you. -->
							<span
								class="pointer-events-none absolute top-1 right-1 hidden size-2 rounded-full bg-warning ring-2 ring-sidebar group-data-[collapsible=icon]:block"
								aria-hidden="true"
							></span>
						{/if}
					</Sidebar.MenuItem>
				</Sidebar.Menu>
			</Sidebar.GroupContent>
		</Sidebar.Group>

		<div class="group-data-[collapsible=icon]:hidden">
			<SessionList {current} onnavigate={close} />
		</div>
	</Sidebar.Content>

	<Sidebar.Footer class="border-t pb-[max(0.5rem,env(safe-area-inset-bottom))]">
		<Sidebar.Menu>
			<Sidebar.MenuItem>
				<Sidebar.MenuButton
					tooltipContent="Settings ({keys('mod+.')}) · {access}"
					aria-label="Settings"
					onclick={() => ui.openSettings(viewport.wide ? 'general' : '')}
				>
					<Settings />
					<span>Settings</span>
				</Sidebar.MenuButton>
				<Sidebar.MenuBadge class="gap-1.5 font-normal text-muted-foreground" data-access>
					{#if app.fullAccess}
						<ShieldAlert class="size-3.5 text-destructive" />
					{:else}
						<span
							class="size-1.5 rounded-full {app.canControl
								? 'bg-success'
								: 'bg-muted-foreground/50'}"
							aria-hidden="true"
						></span>
					{/if}
					{access}
				</Sidebar.MenuBadge>
			</Sidebar.MenuItem>
		</Sidebar.Menu>
	</Sidebar.Footer>
	<Sidebar.Rail />
</Sidebar.Root>
