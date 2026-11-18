<script lang="ts">
	// Going back to an earlier prompt: pick the prompt (unless a message's button already did), then
	// what to restore: the conversation, the files, or both.
	import type { Session } from '$lib/session/session.svelte';
	import type { TreeEntry } from '$lib/session/tree';
	import { ui } from '$lib/ui.svelte';
	import { toasts } from '$lib/toasts.svelte';
	import { ago } from '$lib/format';
	import Sheet from './Sheet.svelte';

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

	let prompts = $state.raw<TreeEntry[] | null>(null);
	let error = $state<string | null>(null);
	let what = $state<(typeof WHAT)[number][0]>('both');
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
				prompts = t.filter((e) => e.current !== false && e.role === 'user' && e.text).reverse();
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
</script>

<Sheet
	{open}
	title="Rewind"
	subtitle={chosen ? 'Go back to before this prompt' : 'Choose the prompt to go back to'}
	onclose={() => (ui.rewind = null)}
	onback={chosen && prompts ? () => (ui.rewind = {}) : undefined}
>
	{#if chosen}
		<div class="space-y-4">
			{#if ui.rewind?.text}
				<blockquote
					class="bg-bubble text-bubble-foreground line-clamp-3 rounded-2xl px-3.5 py-2.5 text-sm leading-relaxed"
				>
					{ui.rewind.text}
				</blockquote>
			{/if}
			<div role="radiogroup" aria-label="What to restore" class="space-y-1">
				{#each WHAT as [v, label, hint] (v)}
					<button
						type="button"
						role="radio"
						aria-checked={what === v}
						class="choice"
						onclick={() => (what = v)}
					>
						<span class="min-w-0 flex-1">
							<span class="block text-sm font-medium">{label}</span>
							<span class="text-muted-foreground mt-0.5 block text-xs leading-snug">{hint}</span>
						</span>
					</button>
				{/each}
			</div>
			<div class="flex justify-end gap-2">
				<button type="button" class="btn btn-secondary" onclick={() => (ui.rewind = null)}
					>Cancel</button
				>
				<button type="button" class="btn btn-primary" disabled={busy} onclick={go}>Rewind</button>
			</div>
		</div>
	{:else if error}
		<p class="text-muted-foreground py-6 text-sm">Rewind is not available from this device.</p>
	{:else if !prompts}
		<p class="shimmer py-6 text-sm">Reading the session</p>
	{:else if !prompts.length}
		<p class="text-muted-foreground py-6 text-sm">There is no earlier prompt to go back to.</p>
	{:else}
		<ul class="-mx-2" aria-label="Prompts">
			{#each prompts as p (p.id)}
				<li>
					<button
						type="button"
						class="opt"
						onclick={() => (ui.rewind = { entryId: p.id, text: p.text ?? '' })}
					>
						<span class="min-w-0 flex-1 truncate">{p.text}</span>
						{#if p.timestamp}<span class="text-faint num shrink-0 text-xs">{ago(p.timestamp)}</span
							>{/if}
					</button>
				</li>
			{/each}
		</ul>
	{/if}
</Sheet>
