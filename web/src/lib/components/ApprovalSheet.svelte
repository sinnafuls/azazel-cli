<script lang="ts">
	// A tool call that waits for a yes or a no: what it is, what it would change, and the answers.
	import type { OpenRequest } from '$lib/session/reduce';
	import type { Decision } from '$lib/protocol';
	import { previewDiff, prettyInput, summarize } from '$lib/tools';
	import Sheet from './Sheet.svelte';
	import DiffView from './DiffView.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Textarea } from '$lib/components/ui/textarea';
	import * as Field from '$lib/components/ui/field';
	import * as Alert from '$lib/components/ui/alert';
	import * as CodeBlock from '$lib/components/ui/code-block';

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
	const HOLD_MS = 1200;

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
		}, HOLD_MS);
	}
	function holdEnd() {
		holding = false;
		if (holdTimer) clearTimeout(holdTimer);
		holdTimer = null;
	}
	const uid = $props.id();
</script>

<Sheet
	open={true}
	docked
	attention
	title="{f.job
		? `Script ${f.job} asks to use`
		: f.agent === 'lead'
			? 'Allow'
			: `${f.agent} asks to use`} {f.tool}?"
	subtitle={ACCESS[f.access] ?? f.access}
	dismissible={false}
>
	<div class="grid gap-3" data-request="approval">
		{#if f.destructive || f.secret || f.job || f.agent !== 'lead'}
			<div class="flex flex-wrap items-center gap-2">
				{#if f.destructive}<Badge variant="destructive">Destroys work</Badge>{/if}
				{#if f.secret}<Badge variant="destructive">Secret file</Badge>{/if}
				{#if f.agent !== 'lead'}<Badge variant="outline" class="font-mono">{f.agent}</Badge>{/if}
				{#if f.job}<Badge variant="outline" class="font-mono">{f.job}</Badge>{/if}
			</div>
		{/if}

		{#if f.subjects?.length}
			<ul class="grid gap-0.5 font-mono text-xs leading-relaxed break-all">
				{#each f.subjects as s (s)}<li>{s}</li>{/each}
			</ul>
		{:else}
			<p class="font-mono text-xs leading-relaxed break-all">{summarize(f.tool, f.input)}</p>
		{/if}

		{#if f.note}<p class="text-sm text-warning">{f.note}</p>{/if}

		{#if diff}
			<DiffView {diff} max={14} />
		{:else}
			<CodeBlock.Root size="sm">
				<CodeBlock.Content code={prettyInput(f.input, 900)} class="scroll-thin max-h-40" />
			</CodeBlock.Root>
		{/if}

		{#if f.rule && canControl && !denying}
			<p class="text-xs text-muted-foreground" data-approval-rule>
				{f.secret ? 'This session' : 'This session and Always'} will allow:
				<span class="font-mono break-all text-foreground">{f.rule}</span>
			</p>
		{/if}

		{#if denying}
			<Field.Field>
				<Field.Label for="{uid}-why">Tell the model why (optional)</Field.Label>
				<Textarea
					id="{uid}-why"
					bind:value={feedback}
					placeholder="Use a temporary directory instead."
				/>
			</Field.Field>
		{/if}
		{#if error}
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{error}</Alert.Description>
			</Alert.Root>
		{/if}
		{#if !canControl}
			<p class="text-sm text-muted-foreground">
				Waiting for an answer from someone with a control link.
			</p>
		{/if}
	</div>

	<!-- In a row the main answer is last, at the right; stacked on a phone it is on top. -->
	{#snippet footer()}
		{#if !canControl}
			<p class="mr-auto text-xs text-muted-foreground">View link: you can watch but not answer.</p>
		{:else if denying}
			<Button variant="outline" onclick={() => (denying = false)}>Back</Button>
			<Button
				variant="destructive"
				disabled={busy}
				onclick={() => answer('deny', feedback.trim() || undefined)}>Deny</Button
			>
		{:else}
			<Button
				variant="destructive"
				class="sm:mr-auto"
				disabled={busy}
				onclick={() => (denying = true)}>Deny…</Button
			>
			{#if f.rule}
				{#if !f.secret}
					<Button variant="outline" disabled={busy} onclick={() => answer('always')}>Always</Button>
				{/if}
				<Button variant="outline" disabled={busy} onclick={() => answer('session')}
					>This session</Button
				>
			{/if}
			{#if f.destructive}
				<Button
					class="relative touch-none overflow-hidden select-none sm:min-w-36"
					disabled={busy}
					data-holding={holding ? '' : undefined}
					onpointerdown={holdStart}
					onpointerup={holdEnd}
					onpointerleave={holdEnd}
					onpointercancel={holdEnd}
					oncontextmenu={(e) => e.preventDefault()}
					onkeydown={(e) => {
						if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) holdStart();
					}}
					onkeyup={holdEnd}
					onblur={holdEnd}
				>
					<!-- Held: the wash fills the button from the left, then the call is allowed. -->
					<span
						class="pointer-events-none absolute inset-0 origin-left bg-primary-foreground/30 transition-transform ease-linear {holding
							? 'scale-x-100'
							: 'scale-x-0'}"
						style="transition-duration: {holding ? HOLD_MS : 120}ms"
						aria-hidden="true"
					></span>
					<span class="relative">{holding ? 'Keep holding…' : 'Hold to allow'}</span>
				</Button>
			{:else}
				<Button class="sm:min-w-36" disabled={busy} onclick={() => answer('allow')}
					>Allow once</Button
				>
			{/if}
		{/if}
	{/snippet}
</Sheet>
