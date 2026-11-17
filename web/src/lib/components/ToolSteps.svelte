<script lang="ts">
	// A run of tool calls in the transcript: one quiet row ("Ran 3 commands") that opens a sheet
	// with every call on a timeline, and each call in full a step further in.
	import type { ToolItem } from '$lib/session/reduce';
	import { describeSteps, diffStats, previewDiff, stepVerb, summarize, toolKind } from '$lib/tools';
	import Sheet from './Sheet.svelte';
	import ToolDetail from './ToolDetail.svelte';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import ListChecks from '@lucide/svelte/icons/list';
	import Terminal from '@lucide/svelte/icons/terminal';
	import FilePlus from '@lucide/svelte/icons/file-plus';
	import FilePen from '@lucide/svelte/icons/file-pen';
	import FileText from '@lucide/svelte/icons/file-text';
	import Search from '@lucide/svelte/icons/search';
	import Globe from '@lucide/svelte/icons/globe';
	import Users from '@lucide/svelte/icons/users';
	import Wrench from '@lucide/svelte/icons/wrench';
	import LoaderCircle from '@lucide/svelte/icons/loader-circle';

	let { tools }: { tools: ToolItem[] } = $props();

	const ICONS = {
		command: Terminal,
		read: FileText,
		create: FilePlus,
		edit: FilePen,
		search: Search,
		web: Globe,
		agents: Users,
		other: Wrench
	} as const;
	const iconOf = (t: ToolItem) => ICONS[toolKind(t.name)];
	const isRunning = (t: ToolItem) => t.status === 'running' || t.status === 'pending';
	const statsOf = (t: ToolItem) => diffStats(t.diff ?? previewDiff(t.name, t.input));

	const one = $derived(tools.length === 1 ? tools[0] : null);
	const running = $derived(tools.find(isRunning) ?? null);
	const failures = $derived(tools.filter((t) => t.status === 'error').length);
	const label = $derived(
		one
			? stepVerb(one.name, isRunning(one))
			: describeSteps(tools.map((t) => t.name)) + (running ? '…' : '')
	);
	const target = $derived(one ? summarize(one.name, one.input) : '');
	const oneStats = $derived(one ? statsOf(one) : null);
	// The last lines of a command that is still running, under the row.
	const tail = $derived(
		running && running.name === 'shell' && running.live
			? running.live.trimEnd().split('\n').slice(-2).join('\n')
			: ''
	);

	let open = $state(false);
	let shown = $state<string | null>(null);
	const detail = $derived(tools.find((t) => t.id === shown) ?? null);
	function openSheet() {
		shown = one ? one.id : null;
		open = true;
	}
	const sheetTitle = $derived(
		detail ? stepVerb(detail.name, isRunning(detail)) : describeSteps(tools.map((t) => t.name))
	);
	const RowIcon = $derived(one ? iconOf(one) : ListChecks);
</script>

<div data-steps={tools.length}>
	<button
		type="button"
		class="step {failures ? 'text-destructive' : ''}"
		aria-haspopup="dialog"
		onclick={openSheet}
	>
		<RowIcon
			class="size-[18px] shrink-0 {failures ? 'text-destructive' : running ? 'text-info' : ''}"
			aria-hidden="true"
		/>
		<span class="flex min-w-0 flex-1 items-baseline gap-2">
			<span class="shrink-0 text-[15px]">{label}</span>
			{#if target}<span class="text-faint min-w-0 truncate font-mono text-xs">{target}</span>{/if}
		</span>
		<span class="flex shrink-0 items-center gap-2">
			{#if oneStats && (oneStats.add || oneStats.del)}
				<span class="meta"
					><span class="text-diff-add-fg">+{oneStats.add}</span>
					<span class="text-diff-del-fg">−{oneStats.del}</span></span
				>
			{/if}
			{#if running}
				<LoaderCircle class="text-info size-4 animate-spin" aria-label="running" />
			{:else if failures}
				<span class="tag tag-danger">{one ? 'failed' : `${failures} failed`}</span>
			{/if}
			<ChevronRight class="text-faint size-4" aria-hidden="true" />
		</span>
	</button>
	{#if tail}
		<pre
			class="text-faint ml-[30px] line-clamp-2 overflow-hidden font-mono text-xs leading-5 whitespace-pre-wrap">{tail}</pre>
	{/if}
</div>

<Sheet
	{open}
	title={sheetTitle}
	subtitle={detail ? undefined : `${tools.length} step${tools.length === 1 ? '' : 's'}`}
	onclose={() => (open = false)}
	onback={detail && !one ? () => (shown = null) : undefined}
>
	{#if detail}
		<ToolDetail tool={detail} />
	{:else}
		<ol class="relative space-y-1" aria-label="Steps">
			<span class="bg-border-strong absolute top-6 bottom-6 left-[21px] w-px" aria-hidden="true"
			></span>
			{#each tools as t (t.id)}
				{@const Icon = iconOf(t)}
				{@const s = statsOf(t)}
				{@const failed = t.status === 'error'}
				<li class="relative">
					<button
						type="button"
						class="opt min-h-16 gap-3.5 rounded-2xl"
						data-tool={t.name}
						data-status={t.status}
						onclick={() => (shown = t.id)}
					>
						<span
							class="bg-overlay relative grid size-6 shrink-0 place-items-center {failed
								? 'text-destructive'
								: isRunning(t)
									? 'text-info'
									: 'text-muted-foreground'}"
						>
							{#if isRunning(t)}<LoaderCircle class="size-[18px] animate-spin" />{:else}<Icon
									class="size-[18px]"
								/>{/if}
						</span>
						<span class="min-w-0 flex-1">
							<span class="block text-[15px] {failed ? 'text-destructive' : ''}"
								>{stepVerb(t.name, isRunning(t))}{failed ? ' (failed)' : ''}</span
							>
							<span class="text-muted-foreground block truncate font-mono text-xs"
								>{summarize(t.name, t.input) || t.name}</span
							>
						</span>
						{#if s && (s.add || s.del)}<span class="meta shrink-0"
								><span class="text-diff-add-fg">+{s.add}</span>
								<span class="text-diff-del-fg">−{s.del}</span></span
							>{/if}
						<ChevronRight class="text-faint size-4 shrink-0" aria-hidden="true" />
					</button>
				</li>
			{/each}
		</ol>
	{/if}
</Sheet>
