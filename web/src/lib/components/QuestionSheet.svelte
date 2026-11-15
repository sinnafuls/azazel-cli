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
	<div class="space-y-3.5" data-request="ui" data-kind={f.kind}>
		{#if isPlan}
			<p class="text-muted-foreground font-mono text-xs break-all">{f.path}</p>
			<div class="bg-inset scroll-thin max-h-[42dvh] overflow-y-auto rounded-md border p-3.5">
				<Markdown text={String(f.plan)} />
			</div>
			{#if revising}
				<textarea
					class="bg-inset focus-visible:ring-ring min-h-24 w-full rounded-md border p-2.5 text-sm outline-none focus-visible:ring-2"
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
							class="flex min-h-9 shrink-0 items-center gap-1.5 rounded-md border px-3 text-[13px] {tab ===
							i
								? 'bg-raised border-input font-medium'
								: 'text-muted-foreground'}"
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

			{#if q.context}<p
					class="text-muted-foreground text-[13px] leading-relaxed whitespace-pre-wrap"
				>
					{q.context}
				</p>{/if}
			<p class="text-[15px] leading-snug font-medium whitespace-pre-wrap">{q.question}</p>
			{#if q.diff}
				<DiffView diff={q.diff} max={12} />
			{:else if q.code}
				<pre
					class="bg-inset scroll-thin max-h-40 overflow-auto rounded-md border p-2.5 font-mono text-xs leading-5">{q.code}</pre>
			{/if}

			{#if q.expects === 'confirm'}
				<div class="grid grid-cols-2 gap-2" role="radiogroup" aria-label={q.question}>
					{#each [[true, 'Yes'], [false, 'No']] as [v, label] (label)}
						<button
							type="button"
							role="radio"
							aria-checked={d.confirm === v}
							class="relative min-h-11 rounded-lg border px-3 text-sm font-medium {d.confirm === v
								? 'border-primary bg-primary/15'
								: 'hover:bg-raised'}"
							onclick={() => (d.confirm = v as boolean)}
						>
							{label}
							{#if (q.recommended === 'yes') === v && q.recommended}<span
									class="text-muted-foreground block text-[11px] font-normal">suggested</span
								>{/if}
						</button>
					{/each}
				</div>
			{:else if q.expects === 'select' || q.expects === 'multiselect'}
				<ul
					class="space-y-1.5"
					role={q.expects === 'select' ? 'radiogroup' : 'group'}
					aria-label={q.question}
				>
					{#each q.options as opt (opt)}
						{@const on = d.chosen.includes(opt)}
						<li>
							<button
								type="button"
								role={q.expects === 'select' ? 'radio' : 'checkbox'}
								aria-checked={on}
								class="flex min-h-11 w-full items-center gap-3 rounded-lg border px-3 py-2 text-left text-sm {on
									? 'border-primary bg-primary/15'
									: 'hover:bg-raised'}"
								onclick={() => pick(opt)}
							>
								<span
									class="grid size-4 shrink-0 place-items-center border {q.expects === 'select'
										? 'rounded-full'
										: 'rounded-sm'} {on
										? 'border-primary bg-primary text-primary-foreground'
										: 'border-input'}"
									aria-hidden="true"
								>
									{#if on}<Check class="size-3" />{/if}
								</span>
								<span class="min-w-0 flex-1 break-words">{opt}</span>
								{#if q.recommended === opt}<span class="text-accent-text shrink-0 text-[11px]"
										>suggested</span
									>{/if}
							</button>
						</li>
					{/each}
				</ul>
			{/if}

			{#if q.expects === 'text' || q.allow_freeform}
				<label class="block">
					{#if q.allow_freeform}<span class="text-muted-foreground mb-1 block text-xs"
							>Or write your own answer</span
						>{/if}
					<input
						class="bg-inset focus-visible:ring-ring h-11 w-full rounded-md border px-3 text-sm outline-none focus-visible:ring-2 md:h-9"
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
		{#if error}<p class="text-destructive text-[13px]" role="alert">{error}</p>{/if}
	</div>

	{#snippet footer()}
		{#if !canControl}
			<p class="text-muted-foreground text-center text-xs">
				View link: waiting for someone with a control link to answer.
			</p>
		{:else if isPlan}
			{#if revising}
				<div class="flex gap-2">
					<Button variant="outline" class="h-11 flex-1 md:h-9" onclick={() => (revising = false)}
						>Back</Button
					>
					<Button
						class="h-11 flex-1 md:h-9"
						disabled={busy || !feedback.trim()}
						onclick={() => send(feedback.trim())}>Send feedback</Button
					>
				</div>
			{:else}
				<div class="flex gap-2">
					<Button variant="outline" class="h-11 flex-1 md:h-9" onclick={() => (revising = true)}
						>Ask for changes</Button
					>
					<Button class="h-11 flex-1 md:h-9" disabled={busy} onclick={() => send(true)}
						>Approve plan</Button
					>
				</div>
			{/if}
		{:else}
			<div class="flex gap-2">
				<Button variant="outline" class="h-11 md:h-9" disabled={busy} onclick={() => send(null)}
					>Dismiss</Button
				>
				{#if multiple && tab < questions.length - 1}
					<Button variant="secondary" class="h-11 flex-1 md:h-9" onclick={() => tab++}>Next</Button>
				{/if}
				{#if !multiple || tab === questions.length - 1 || drafts.some((_, i) => answerOf(questions[i], drafts[i]) !== null)}
					<Button class="h-11 flex-1 md:h-9" disabled={busy || !ready} onclick={() => void submit()}
						>Send answer</Button
					>
				{/if}
			</div>
		{/if}
	{/snippet}
</Sheet>
