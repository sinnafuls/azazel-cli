<script lang="ts">
	import type { OpenRequest } from '$lib/session/reduce';
	import type { Decision } from '$lib/protocol';
	import { previewDiff, prettyInput, summarize } from '$lib/tools';
	import Sheet from './Sheet.svelte';
	import DiffView from './DiffView.svelte';
	import { Button } from '$lib/components/ui/button';

	type Approval = Extract<OpenRequest, { kind: 'approval' }>;
	let {
		request,
		canControl,
		onanswer
	}: {
		request: Approval;
		canControl: boolean;
		onanswer: (decision: Decision, feedback?: string) => Promise<void>;
	} = $props();

	const f = $derived(request.frame);
	const diff = $derived(previewDiff(f.tool, f.input));
	let denying = $state(false);
	let feedback = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let holding = $state(false);
	let holdTimer: ReturnType<typeof setTimeout> | null = null;

	const ACCESS: Record<string, string> = {
		read: 'Reads',
		write: 'Writes',
		exec: 'Runs',
		net: 'Network'
	};

	async function answer(d: Decision, fb?: string) {
		if (busy) return;
		busy = true;
		error = null;
		try {
			await onanswer(d, fb);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			busy = false;
		}
	}

	// Destructive calls need a deliberate press: hold for 1.2 s. Space and Enter do not shortcut it.
	function holdStart() {
		if (busy) return;
		holding = true;
		holdTimer = setTimeout(() => {
			holding = false;
			void answer('allow');
		}, 1200);
	}
	function holdEnd() {
		holding = false;
		if (holdTimer) clearTimeout(holdTimer);
		holdTimer = null;
	}
</script>

<Sheet
	open={true}
	docked
	attention
	title="{f.agent === 'lead' ? 'Allow' : `${f.agent} asks to use`} {f.tool}?"
	subtitle={ACCESS[f.access] ?? f.access}
	dismissible={false}
>
	<div class="space-y-3" data-request="approval">
		<div class="flex flex-wrap items-center gap-1.5">
			<span
				class="bg-raised rounded-md border px-1.5 py-0.5 font-mono text-[10.5px] tracking-wide uppercase"
				>{f.access}</span
			>
			{#if f.destructive}<span
					class="border-destructive/50 text-destructive rounded-md border px-1.5 py-0.5 text-[11px] font-medium"
					>Destroys work</span
				>{/if}
			{#if f.secret}<span
					class="border-destructive/50 text-destructive rounded-md border px-1.5 py-0.5 text-[11px] font-medium"
					>Secret file</span
				>{/if}
			{#if f.agent !== 'lead'}<span class="text-muted-foreground font-mono text-[11px]"
					>{f.agent}</span
				>{/if}
		</div>

		{#if f.subjects?.length}
			<ul class="space-y-0.5 font-mono text-[13px] break-all">
				{#each f.subjects as s (s)}<li>{s}</li>{/each}
			</ul>
		{:else}
			<p class="font-mono text-[13px] break-all">{summarize(f.tool, f.input)}</p>
		{/if}

		{#if f.note}<p class="text-accent-text text-[13px]">{f.note}</p>{/if}

		{#if diff}
			<DiffView {diff} max={14} />
		{:else}
			<pre
				class="bg-inset scroll-thin max-h-40 overflow-auto rounded-md border p-2.5 font-mono text-xs leading-5 whitespace-pre-wrap break-words">{prettyInput(
					f.input,
					900
				)}</pre>
		{/if}

		{#if denying}
			<label class="block">
				<span class="text-muted-foreground mb-1 block text-xs">Tell the model why (optional)</span>
				<textarea
					class="bg-inset focus-visible:ring-ring min-h-20 w-full rounded-md border p-2.5 text-sm outline-none focus-visible:ring-2"
					bind:value={feedback}
					placeholder="Use a temporary directory instead."></textarea>
			</label>
		{/if}
		{#if error}<p class="text-destructive text-[13px]" role="alert">{error}</p>{/if}
		{#if !canControl}<p class="text-muted-foreground text-[13px]">
				Waiting for an answer from someone with a control link.
			</p>{/if}
	</div>

	{#snippet footer()}
		{#if canControl}
			{#if denying}
				<div class="flex gap-2">
					<Button variant="outline" class="h-11 flex-1 md:h-9" onclick={() => (denying = false)}
						>Back</Button
					>
					<Button
						variant="destructive"
						class="h-11 flex-1 md:h-9"
						disabled={busy}
						onclick={() => answer('deny', feedback.trim() || undefined)}>Deny</Button
					>
				</div>
			{:else}
				<div class="grid grid-cols-2 gap-2">
					{#if f.destructive}
						<button
							type="button"
							class="bg-primary text-primary-foreground focus-visible:ring-ring relative col-span-2 h-11 touch-none overflow-hidden rounded-lg text-sm font-medium select-none focus-visible:ring-2 disabled:opacity-50 md:h-9"
							disabled={busy}
							onpointerdown={holdStart}
							onpointerup={holdEnd}
							onpointerleave={holdEnd}
							onpointercancel={holdEnd}
							onkeydown={(e) => {
								if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) holdStart();
							}}
							onkeyup={holdEnd}
							onblur={holdEnd}
						>
							<span
								class="bg-primary-foreground/25 absolute inset-0 origin-left {holding
									? 'animate-[hold_1.2s_linear_forwards]'
									: 'scale-x-0'}"
							></span>
							<span class="relative">{holding ? 'Keep holding…' : 'Hold to allow'}</span>
						</button>
					{:else}
						<Button class="col-span-2 h-11 md:h-9" disabled={busy} onclick={() => answer('allow')}
							>Allow once</Button
						>
					{/if}
					{#if f.rule}
						<Button
							variant="outline"
							class="h-11 md:h-9"
							disabled={busy}
							onclick={() => answer('session')}
							title={f.rule}>This session</Button
						>
						{#if !f.secret}
							<Button
								variant="outline"
								class="h-11 md:h-9"
								disabled={busy}
								onclick={() => answer('always')}
								title={f.rule}>Always</Button
							>
						{/if}
					{/if}
					<Button
						variant="destructive"
						class="{f.rule && !f.secret ? 'col-span-2' : 'col-span-2'} h-11 md:h-9"
						disabled={busy}
						onclick={() => (denying = true)}>Deny…</Button
					>
				</div>
			{/if}
		{:else}
			<p class="text-muted-foreground text-center text-xs">
				View link: you can watch but not answer.
			</p>
		{/if}
	{/snippet}
</Sheet>
