<script lang="ts">
	// The session's profile: the same ghost Button as the other chips of the status bar, opening a
	// DropdownMenu of the profiles there are. A profile sets the models of every role at once, so
	// each row names the three that tell profiles apart. One that lacks a login is listed but
	// cannot be chosen, and says which `azazel login` it needs.
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import { cn } from '$lib/utils';
	import type { Session } from '$lib/session/session.svelte';
	import { profileLogins, profileSource, profileSummary } from '$lib/profiles';
	import ChevronDown from '@lucide/svelte/icons/chevron-down';
	import Layers from '@lucide/svelte/icons/layers';

	let {
		session,
		canControl,
		open = $bindable(false),
		onpick,
		class: className
	}: {
		session: Session;
		canControl: boolean;
		open?: boolean;
		/** A profile was chosen; the status bar switches to it and closes the menu. */
		onpick: (name: string) => void;
		class?: string;
	} = $props();

	const st = $derived(session.state);
	const profiles = $derived(st?.profiles ?? []);
</script>

<!-- A computer without profiles (an older azazel) sends none: there is nothing to show. -->
{#if profiles.length}
	<DropdownMenu.Root bind:open>
		<DropdownMenu.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="ghost"
					size="sm"
					class={className}
					disabled={!canControl}
					aria-label="Profile: {st?.profile ?? 'none'}"
				>
					<Layers />
					<span class="truncate">{st?.profile ?? 'no profile'}</span>
					{#if canControl}<ChevronDown class="size-3 opacity-60" />{/if}
				</Button>
			{/snippet}
		</DropdownMenu.Trigger>
		<DropdownMenu.Content side="top" align="start" class="w-md max-w-[calc(100vw-1.5rem)]">
			<DropdownMenu.Label>Profile</DropdownMenu.Label>
			<DropdownMenu.RadioGroup value={st?.profile ?? ''} onValueChange={onpick}>
				{#each profiles as p (p.name)}
					<!-- The reason an unusable profile is off stays readable: only its name is dimmed. -->
					<DropdownMenu.RadioItem
						value={p.name}
						disabled={!p.usable}
						class="data-disabled:opacity-100"
						data-profile={p.name}
					>
						<span class="grid min-w-0 flex-1 gap-0.5">
							<span class="flex min-w-0 items-center gap-1.5">
								<span class={cn('truncate font-medium', !p.usable && 'opacity-50')}>{p.name}</span>
								<Badge variant="outline" class="font-normal text-muted-foreground"
									>{profileSource(p)}</Badge
								>
							</span>
							<!-- One line for every profile, so the rows of the list are one height. -->
							{#if p.usable}
								<span class="truncate font-mono text-xs text-muted-foreground"
									>{profileSummary(p) || 'sets other roles only'}</span
								>
							{:else}
								<span class="truncate text-xs text-muted-foreground"
									>needs <code class="font-mono">{profileLogins(p).join(', ')}</code></span
								>
							{/if}
						</span>
					</DropdownMenu.RadioItem>
				{/each}
			</DropdownMenu.RadioGroup>
			<DropdownMenu.Separator />
			<DropdownMenu.Label class="font-normal">
				Each line names the models of @default, @slow and @smol. After a switch the next reply
				starts a new prompt cache.
			</DropdownMenu.Label>
		</DropdownMenu.Content>
	</DropdownMenu.Root>
{/if}
