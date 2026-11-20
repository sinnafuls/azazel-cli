<script lang="ts">
	// Going back to an earlier prompt: pick the prompt (unless a message's button already did), then
	// what to restore: the conversation, the files, or both.
	import type { Session } from '$lib/session/session.svelte';
	import { rewindable, type TreeEntry } from '$lib/session/tree';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { ago } from '$lib/format';
	import Sheet from './Sheet.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Skeleton } from '$lib/components/ui/skeleton';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import * as Field from '$lib/components/ui/field';
	import * as Item from '$lib/components/ui/item';
	import * as Empty from '$lib/components/ui/empty';

	let { session }: { session: Session } = $props();

	const WHAT = [
		['both', 'Conversation and files', 'Forget what was said since, and put the files back.'],
		[
			'conversation',
			'Conversation only',
			'Forget what was said since. The files stay as they are.'
		],
		['code', 'Files only', 'Put the files back. The conversation stays.']
	] as const;
	type What = (typeof WHAT)[number][0];

	let prompts = $state.raw<TreeEntry[] | null>(null);
	let error = $state<string | null>(null);
	let what = $state<What>('both');
	let busy = $state(false);
	const open = $derived(ui.rewind !== null);
	const chosen = $derived(ui.rewind?.entryId ?? null);

	$effect(() => {
		if (!open || chosen) return;
		prompts = null;
		error = null;
		session
			.tree()
			.then((t) => {
				prompts = rewindable(t).reverse();
			})
			.catch((e) => (error = e instanceof Error ? e.message : String(e)));
	});

	async function go() {
		if (!chosen) return;
		busy = true;
		try {
			await session.rewind(chosen, what);
			toasts.push('Went back to before that prompt.', 'success');
			ui.rewind = null;
		} catch (e) {
			toasts.push(e instanceof Error ? e.message : String(e), 'error');
		} finally {
			busy = false;
		}
	}
	const uid = $props.id();
</script>

{#snippet actions()}
	<Button variant="outline" onclick={() => (ui.rewind = null)}>Cancel</Button>
	<Button disabled={busy} onclick={go}>Rewind</Button>
{/snippet}

<Sheet
	{open}
	title="Rewind"
	subtitle={chosen ? 'Go back to before this prompt' : 'Choose the prompt to go back to'}
	onclose={() => (ui.rewind = null)}
	onback={chosen && prompts ? () => (ui.rewind = {}) : undefined}
	footer={chosen ? actions : undefined}
>
	{#if chosen}
		<div class="grid gap-4">
			{#if ui.rewind?.text}
				<!-- The clamp is on the text, not on the padded box: a clamped box still lays out the
				     lines it leaves out, and the top of the fourth showed in the padding below the third. -->
				<blockquote
					class="rounded-xl bg-secondary px-3 py-2 text-sm leading-relaxed text-secondary-foreground"
				>
					<p class="line-clamp-3">{ui.rewind.text}</p>
				</blockquote>
			{/if}
			<RadioGroup.Root
				aria-label="What to restore"
				value={what}
				onValueChange={(v) => (what = v as What)}
			>
				{#each WHAT as [v, label, hint] (v)}
					<Field.Label for="{uid}-{v}">
						<Field.Field orientation="horizontal">
							<RadioGroup.Item value={v} id="{uid}-{v}" />
							<Field.Content>
								<Field.Title>{label}</Field.Title>
								<Field.Description class="text-xs">{hint}</Field.Description>
							</Field.Content>
						</Field.Field>
					</Field.Label>
				{/each}
			</RadioGroup.Root>
		</div>
	{:else if error}
		<Empty.Root class="p-6">
			<Empty.Description>Rewind is not available from this device.</Empty.Description>
		</Empty.Root>
	{:else if !prompts}
		<div class="grid gap-2" role="status" aria-label="Reading the session">
			{#each [0, 1, 2] as i (i)}<Skeleton class="h-8 w-full" />{/each}
		</div>
	{:else if !prompts.length}
		<Empty.Root class="p-6">
			<Empty.Description>There is no earlier prompt to go back to.</Empty.Description>
		</Empty.Root>
	{:else}
		<!-- One column no wider than the sheet: a long prompt ends in an ellipsis, it does not widen the list. -->
		<ul class="-mx-2.5 grid grid-cols-1 gap-0.5" aria-label="Prompts">
			{#each prompts as p (p.id)}
				<li>
					<Item.Root size="xs" class="w-full flex-nowrap text-left hover:bg-muted">
						{#snippet child({ props })}
							<button
								type="button"
								{...props}
								onclick={() => (ui.rewind = { entryId: p.id, text: p.text ?? '' })}
							>
								<Item.Content class="min-w-0">
									<Item.Title class="block w-full truncate font-normal">{p.text}</Item.Title>
								</Item.Content>
								{#if p.timestamp}
									<Item.Actions class="num shrink-0 text-xs text-muted-foreground"
										>{ago(p.timestamp)}</Item.Actions
									>
								{/if}
							</button>
						{/snippet}
					</Item.Root>
				</li>
			{/each}
		</ul>
	{/if}
</Sheet>
