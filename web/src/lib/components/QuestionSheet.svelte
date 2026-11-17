<script lang="ts">
	// Answers every kind of ui_request: ask, form, confirm, select, input, plan.
	import type { OpenRequest } from '$lib/session/reduce';
	import type { Question } from '$lib/protocol';
	import { answerOf, emptyDraft, type Answer, type Draft } from '$lib/answers';
	import Sheet from './Sheet.svelte';
	import Markdown from './Markdown.svelte';
	import DiffView from './DiffView.svelte';
	import { Button } from '$lib/components/ui/button';
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
</script>

<Sheet
	open={true}
	docked
	attention
	{title}
	subtitle={multiple ? `${tab + 1} of ${questions.length}` : undefined}
	dismissible={false}
>
	<div class="space-y-3" data-request="ui" data-kind={f.kind}>
		{#if isPlan}
			<p class="text-muted-foreground meta break-all">{f.path}</p>
			<div class="bg-inset scroll-thin max-h-[42dvh] overflow-y-auto rounded-lg border p-4">
				<Markdown text={String(f.plan)} />
			</div>
			{#if revising}
				<textarea
					class="field min-h-24"
					bind:value={feedback}
					placeholder="What should change?"
					aria-label="Feedback on the plan"></textarea>
			{/if}
		{:else if q && d}
			{#if multiple}
				<div
					class="scroll-thin -mx-1 flex gap-1 overflow-x-auto px-1"
					role="tablist"
					aria-label="Questions"
				>
					{#each questions as qq, i (i)}
						<button
							type="button"
							role="tab"
							aria-selected={tab === i}
							class="flex min-h-11 shrink-0 items-center gap-1.5 rounded-md px-3 text-sm transition-colors duration-100 fine:min-h-8 {tab ===
							i
								? 'bg-raised font-medium'
								: 'text-muted-foreground hover:text-foreground'}"
							onclick={() => (tab = i)}
						>
							{#if answerOf(qq, drafts[i] ?? emptyDraft(qq)) !== null}<Check
									class="text-success size-3.5"
								/>{/if}
							{qq.header || `Question ${i + 1}`}
						</button>
					{/each}
				</div>
			{/if}

			{#if q.context}<p class="text-muted-foreground text-sm leading-relaxed whitespace-pre-wrap">
					{q.context}
				</p>{/if}
			<p class="text-base leading-snug font-medium tracking-[-0.015em] whitespace-pre-wrap">
				{q.question}
			</p>
			{#if q.diff}
				<DiffView diff={q.diff} max={12} />
			{:else if q.code}
				<pre
					class="well scroll-thin max-h-40 overflow-auto border p-3 whitespace-pre">{q.code}</pre>
			{/if}

			{#if q.expects === 'confirm'}
				<div class="grid grid-cols-2 gap-2" role="radiogroup" aria-label={q.question}>
					{#each [[true, 'Yes'], [false, 'No']] as [v, label] (label)}
						<button
							type="button"
							role="radio"
							aria-checked={d.confirm === v}
							class="choice min-h-11 px-3 py-1.5 text-sm font-medium"
							onclick={() => (d.confirm = v as boolean)}
						>
							{label}
							{#if (q.recommended === 'yes') === v && q.recommended}<span
									class="text-muted-foreground block text-xs font-normal">suggested</span
								>{/if}
						</button>
					{/each}
				</div>
			{:else if q.expects === 'select' || q.expects === 'multiselect'}
				<ul
					class="space-y-1.5"
					role={q.expects === 'select' ? 'radiogroup' : 'group'}
					data-options
					aria-label={q.question}
				>
					{#each q.options as opt (opt)}
						{@const on = d.chosen.includes(opt)}
						<li>
							<button
								type="button"
								role={q.expects === 'select' ? 'radio' : 'checkbox'}
								aria-checked={on}
								class="choice flex min-h-11 w-full items-center gap-3 px-3 py-2 text-left text-sm"
								onclick={() => pick(opt)}
							>
								<span
									class="grid size-4 shrink-0 place-items-center border {q.expects === 'select'
										? 'rounded-full'
										: 'rounded-xs'} {on
										? 'border-ember bg-ember text-ember-foreground'
										: 'border-input'}"
									aria-hidden="true"
								>
									{#if on}<Check class="size-3" />{/if}
								</span>
								<span class="min-w-0 flex-1 break-words">{opt}</span>
								{#if q.recommended === opt}<span class="text-muted-foreground shrink-0 text-xs"
										>suggested</span
									>{/if}
							</button>
						</li>
					{/each}
				</ul>
			{/if}

			{#if q.expects === 'text' || q.allow_freeform}
				<label class="block">
					{#if q.allow_freeform}<span class="label mb-1.5 block">Or write your own answer</span
						>{/if}
					<input
						class="field"
						type={secret ? 'password' : 'text'}
						autocomplete="off"
						spellcheck="false"
						aria-label={q.expects === 'text' ? q.question : 'Your own answer'}
						bind:value={d.text}
						onkeydown={(e) => {
							if (e.key === 'Enter' && !multiple && ready && canControl) void submit();
						}}
					/>
				</label>
			{/if}
		{/if}
		{#if error}<p class="text-destructive text-sm" role="alert">{error}</p>{/if}
	</div>

	{#snippet footer()}
		{#if !canControl}
			<p class="text-muted-foreground text-xs">
				View link: waiting for someone with a control link to answer.
			</p>
		{:else if isPlan}
			{#if revising}
				<div class="flex gap-2 md:justify-end">
					<Button variant="outline" class="flex-1 md:flex-none" onclick={() => (revising = false)}
						>Back</Button
					>
					<Button
						class="flex-1 md:flex-none"
						disabled={busy || !feedback.trim()}
						onclick={() => send(feedback.trim())}>Send feedback</Button
					>
				</div>
			{:else}
				<div class="flex gap-2 md:justify-end">
					<Button variant="outline" class="flex-1 md:flex-none" onclick={() => (revising = true)}
						>Ask for changes</Button
					>
					<Button class="flex-1 md:min-w-36 md:flex-none" disabled={busy} onclick={() => send(true)}
						>Approve plan</Button
					>
				</div>
			{/if}
		{:else}
			<div class="flex gap-2">
				<Button variant="ghost" class="md:mr-auto" disabled={busy} onclick={() => send(null)}
					>Dismiss</Button
				>
				{#if multiple && tab < questions.length - 1}
					<Button variant="secondary" class="flex-1 md:flex-none" onclick={() => tab++}>Next</Button
					>
				{/if}
				{#if !multiple || tab === questions.length - 1 || drafts.some((_, i) => answerOf(questions[i], drafts[i]) !== null)}
					<Button
						class="flex-1 md:min-w-36 md:flex-none"
						disabled={busy || !ready}
						onclick={() => void submit()}>Send answer</Button
					>
				{/if}
			</div>
		{/if}
	{/snippet}
</Sheet>
