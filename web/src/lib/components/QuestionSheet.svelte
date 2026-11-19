<script lang="ts">
	// Answers every kind of ui_request: ask, form, confirm, select, input, plan. A choice is a
	// RadioGroup or Checkboxes as choice cards (a Field inside its Label), several questions are Tabs.
	import type { OpenRequest } from '$lib/session/reduce';
	import type { Question } from '$lib/protocol';
	import { answerOf, emptyDraft, type Answer, type Draft } from '$lib/answers';
	import Sheet from './Sheet.svelte';
	import Markdown from './Markdown.svelte';
	import DiffView from './DiffView.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Textarea } from '$lib/components/ui/textarea';
	import { Checkbox } from '$lib/components/ui/checkbox';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import * as Field from '$lib/components/ui/field';
	import * as Tabs from '$lib/components/ui/tabs';
	import * as Alert from '$lib/components/ui/alert';
	import * as CodeBlock from '$lib/components/ui/code-block';
	import Check from '@lucide/svelte/icons/check';

	type Ui = Extract<OpenRequest, { kind: 'ui' }>;
	let {
		request,
		canControl,
		onanswer
	}: { request: Ui; canControl: boolean; onanswer: (value: unknown) => Promise<void> } = $props();

	// One shape for everything the sheet shows: a list of questions.
	type Q = Question & { header?: string | null };
	const f = $derived(request.frame as unknown as Record<string, unknown> & { kind: string });
	const questions = $derived.by((): Q[] => {
		switch (f.kind) {
			case 'ask':
				return [f as unknown as Q];
			case 'form':
				return f.questions as Q[];
			case 'confirm':
				return [
					{ question: f.prompt as string, expects: 'confirm', options: [], multi: false } as Q
				];
			case 'select':
				return [
					{
						question: f.prompt as string,
						expects: 'select',
						options: f.options as string[],
						multi: false
					} as Q
				];
			case 'input':
				return [{ question: f.prompt as string, expects: 'text', options: [], multi: false } as Q];
			default:
				return [];
		}
	});
	const isPlan = $derived(f.kind === 'plan');
	const secret = $derived(f.kind === 'input' && f.secret === true);
	const isLimit = $derived(
		f.kind === 'select' && String(f.prompt).startsWith('Usage limit reached')
	);

	let drafts = $state<Draft[]>([]);
	let tab = $state(0);
	let feedback = $state('');
	let revising = $state(false);
	let busy = $state(false);
	let error = $state<string | null>(null);

	$effect(() => {
		drafts = questions.map((q) => emptyDraft(q));
		tab = 0;
	});

	const q = $derived(questions[tab]);
	const d = $derived(drafts[tab]);
	const multiple = $derived(questions.length > 1);
	const title = $derived(
		isPlan
			? 'Review the plan'
			: multiple
				? 'A few questions'
				: isLimit
					? 'Usage limit reached'
					: 'The agent has a question'
	);

	async function send(value: unknown) {
		if (busy) return;
		busy = true;
		error = null;
		try {
			await onanswer(value);
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			busy = false;
		}
	}

	function submit() {
		if (f.kind === 'form') return send(drafts.map((dr, i) => answerOf(questions[i], dr)));
		const a: Answer = answerOf(questions[0], drafts[0]);
		if (f.kind === 'select') {
			const idx = a && Array.isArray(a) ? (f.options as string[]).indexOf(a[0]) : -1;
			return send(idx >= 0 ? idx : null);
		}
		if (f.kind === 'ask' && questions[0].expects === 'select' && Array.isArray(a)) return send(a);
		return send(a);
	}

	function pick(opt: string) {
		if (!q || !d) return;
		if (q.expects === 'multiselect') {
			d.chosen = d.chosen.includes(opt) ? d.chosen.filter((o) => o !== opt) : [...d.chosen, opt];
		} else {
			d.chosen = [opt];
			d.text = '';
		}
	}

	const ready = $derived(
		isPlan ||
			(f.kind === 'form'
				? true // an open question may stay open
				: questions.length > 0 &&
					answerOf(questions[0], drafts[0] ?? emptyDraft(questions[0])) !== null)
	);
	const uid = $props.id();
</script>

{#snippet choice(label: string, suggested: boolean)}
	<Field.Content>
		<Field.Title class="font-normal break-words">{label}</Field.Title>
		{#if suggested}<Field.Description class="text-xs">suggested</Field.Description>{/if}
	</Field.Content>
{/snippet}

{#snippet body(q: Q, d: Draft)}
	{#if q.context}
		<p class="text-sm leading-relaxed whitespace-pre-wrap text-muted-foreground">{q.context}</p>
	{/if}
	<p class="text-sm leading-snug font-medium whitespace-pre-wrap">{q.question}</p>
	{#if q.diff}
		<DiffView diff={q.diff} max={12} />
	{:else if q.code}
		<CodeBlock.Root size="sm">
			<CodeBlock.Content code={q.code} class="scroll-thin max-h-40" />
		</CodeBlock.Root>
	{/if}

	{#if q.expects === 'confirm'}
		<RadioGroup.Root
			class="grid-cols-2"
			aria-label={q.question}
			value={d.confirm === null ? '' : d.confirm ? 'yes' : 'no'}
			onValueChange={(v) => (d.confirm = v === 'yes')}
		>
			{#each [['yes', 'Yes'], ['no', 'No']] as [v, label] (v)}
				<Field.Label for="{uid}-{v}">
					<Field.Field orientation="horizontal">
						<RadioGroup.Item value={v} id="{uid}-{v}" />
						{@render choice(label, q.recommended === v)}
					</Field.Field>
				</Field.Label>
			{/each}
		</RadioGroup.Root>
	{:else if q.expects === 'select'}
		<RadioGroup.Root
			aria-label={q.question}
			value={d.chosen[0] ?? ''}
			onValueChange={(v) => pick(v)}
			data-options
		>
			{#each q.options as opt, i (opt)}
				<Field.Label for="{uid}-o{i}">
					<Field.Field orientation="horizontal">
						<RadioGroup.Item value={opt} id="{uid}-o{i}" />
						{@render choice(opt, q.recommended === opt)}
					</Field.Field>
				</Field.Label>
			{/each}
		</RadioGroup.Root>
	{:else if q.expects === 'multiselect'}
		<div class="grid gap-2" role="group" aria-label={q.question} data-options>
			{#each q.options as opt, i (opt)}
				<Field.Label for="{uid}-o{i}">
					<Field.Field orientation="horizontal">
						<Checkbox
							id="{uid}-o{i}"
							checked={d.chosen.includes(opt)}
							onCheckedChange={() => pick(opt)}
						/>
						{@render choice(opt, q.recommended === opt)}
					</Field.Field>
				</Field.Label>
			{/each}
		</div>
	{/if}

	{#if q.expects === 'text' || q.allow_freeform}
		<Field.Field>
			{#if q.allow_freeform}
				<Field.Label for="{uid}-text">Or write your own answer</Field.Label>
			{/if}
			<Input
				id="{uid}-text"
				type={secret ? 'password' : 'text'}
				autocomplete="off"
				spellcheck="false"
				aria-label={q.expects === 'text' ? q.question : undefined}
				bind:value={d.text}
				onkeydown={(e) => {
					if (e.key === 'Enter' && !multiple && ready && canControl) void submit();
				}}
			/>
		</Field.Field>
	{/if}
{/snippet}

<Sheet
	open={true}
	docked
	attention
	{title}
	subtitle={multiple ? `${tab + 1} of ${questions.length}` : undefined}
	dismissible={false}
>
	<div class="grid gap-3" data-request="ui" data-kind={f.kind}>
		{#if isPlan}
			<p class="font-mono text-xs break-all text-muted-foreground">{f.path}</p>
			<div class="scroll-thin max-h-[42dvh] overflow-y-auto rounded-lg border p-3">
				<Markdown text={String(f.plan)} />
			</div>
			{#if revising}
				<Textarea
					bind:value={feedback}
					placeholder="What should change?"
					aria-label="Feedback on the plan"
				/>
			{/if}
		{:else if q && d}
			{#if multiple}
				<Tabs.Root value={String(tab)} onValueChange={(v) => (tab = Number(v))}>
					<Tabs.List class="scroll-thin max-w-full justify-start overflow-x-auto">
						{#each questions as qq, i (i)}
							<Tabs.Trigger value={String(i)} class="flex-none">
								{#if answerOf(qq, drafts[i] ?? emptyDraft(qq)) !== null}
									<Check class="text-success" />
								{/if}
								{qq.header || `Question ${i + 1}`}
							</Tabs.Trigger>
						{/each}
					</Tabs.List>
					<Tabs.Content value={String(tab)} class="grid gap-3">
						{@render body(q, d)}
					</Tabs.Content>
				</Tabs.Root>
			{:else}
				{@render body(q, d)}
			{/if}
		{/if}
		{#if error}
			<Alert.Root variant="destructive" role="alert">
				<Alert.Description>{error}</Alert.Description>
			</Alert.Root>
		{/if}
	</div>

	<!-- In a row the main answer is last, at the right; stacked on a phone it is on top. -->
	{#snippet footer()}
		{#if !canControl}
			<p class="mr-auto text-xs text-muted-foreground">
				View link: waiting for someone with a control link to answer.
			</p>
		{:else if isPlan}
			{#if revising}
				<Button variant="outline" onclick={() => (revising = false)}>Back</Button>
				<Button disabled={busy || !feedback.trim()} onclick={() => send(feedback.trim())}
					>Send feedback</Button
				>
			{:else}
				<Button variant="outline" onclick={() => (revising = true)}>Ask for changes</Button>
				<Button class="sm:min-w-36" disabled={busy} onclick={() => send(true)}>Approve plan</Button>
			{/if}
		{:else}
			<Button variant="ghost" class="sm:mr-auto" disabled={busy} onclick={() => send(null)}
				>Dismiss</Button
			>
			{#if multiple && tab < questions.length - 1}
				<Button variant="outline" onclick={() => tab++}>Next</Button>
			{/if}
			{#if !multiple || tab === questions.length - 1 || drafts.some((_, i) => answerOf(questions[i], drafts[i]) !== null)}
				<Button class="sm:min-w-36" disabled={busy || !ready} onclick={() => void submit()}
					>Send answer</Button
				>
			{/if}
		{/if}
	{/snippet}
</Sheet>
