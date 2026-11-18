<script lang="ts" module>
	import type { Component } from 'svelte';
	export type MenuItem =
		| {
				label: string;
				icon?: Component<{ class?: string }>;
				hint?: string;
				danger?: boolean;
				disabled?: boolean;
				onselect: () => void;
		  }
		| 'sep';
	let count = 0;
</script>

<script lang="ts">
	// A button that opens a menu: role=menu, arrows move, Enter picks, Escape or a click elsewhere
	// closes and gives the focus back. It is placed with fixed coordinates so that a scrolling
	// parent (the sidebar) cannot clip it.
	import { tick, type Snippet } from 'svelte';

	let {
		label,
		items,
		class: className = 'icon-btn',
		align = 'end',
		children
	}: {
		label: string;
		items: MenuItem[];
		class?: string;
		align?: 'start' | 'end';
		children: Snippet;
	} = $props();

	const id = `menu-${++count}`;
	let open = $state(false);
	let btn = $state<HTMLButtonElement>();
	let list = $state<HTMLDivElement>();
	let pos = $state({ top: 0, left: 0 });

	function place() {
		if (!btn || !list) return;
		const r = btn.getBoundingClientRect();
		const w = list.offsetWidth;
		const h = list.offsetHeight;
		const left = Math.max(8, Math.min(align === 'end' ? r.right - w : r.left, innerWidth - w - 8));
		let top = r.bottom + 4;
		if (top + h > innerHeight - 8) top = Math.max(8, r.top - h - 4);
		pos = { top, left };
	}
	const rows = () => [
		...(list?.querySelectorAll<HTMLButtonElement>('[role=menuitem]:not(:disabled)') ?? [])
	];
	function focusRow(i: number) {
		const r = rows();
		if (r.length) r[(i + r.length) % r.length].focus();
	}
	async function show(last = false) {
		open = true;
		await tick();
		place();
		focusRow(last ? -1 : 0);
	}
	function hide(refocus = true) {
		if (!open) return;
		open = false;
		if (refocus) btn?.focus();
	}
	function key(e: KeyboardEvent) {
		const at = rows().indexOf(document.activeElement as HTMLButtonElement);
		if (e.key === 'ArrowDown') focusRow(at + 1);
		else if (e.key === 'ArrowUp') focusRow(at < 0 ? -1 : at - 1);
		else if (e.key === 'Home') focusRow(0);
		else if (e.key === 'End') focusRow(-1);
		else if (e.key === 'Escape') {
			e.stopPropagation();
			hide();
		} else if (e.key === 'Tab') {
			hide(false);
			return;
		} else return;
		e.preventDefault();
	}
</script>

<svelte:window onresize={() => hide(false)} />

<button
	bind:this={btn}
	type="button"
	class={className}
	aria-label={label}
	title={label}
	aria-haspopup="menu"
	aria-expanded={open}
	aria-controls={open ? id : undefined}
	onclick={(e) => {
		e.preventDefault();
		e.stopPropagation();
		if (open) hide();
		else void show();
	}}
	onkeydown={(e) => {
		if (!open && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
			e.preventDefault();
			void show(e.key === 'ArrowUp');
		}
	}}
>
	{@render children()}
</button>

{#if open}
	<div
		class="fixed inset-0 z-40"
		aria-hidden="true"
		onpointerdown={(e) => {
			e.preventDefault();
			hide(false);
		}}
	></div>
	<!-- svelte-ignore a11y_interactive_supports_focus -->
	<div
		bind:this={list}
		{id}
		role="menu"
		aria-label={label}
		class="pop fixed z-50 min-w-44"
		style="top: {pos.top}px; left: {pos.left}px"
		onkeydown={key}
	>
		{#each items as item, i (i)}
			{#if item === 'sep'}
				<div class="pop-sep" role="separator"></div>
			{:else}
				<button
					type="button"
					role="menuitem"
					tabindex="-1"
					class="pop-item {item.danger ? 'is-danger' : ''}"
					disabled={item.disabled}
					onclick={() => {
						hide();
						item.onselect();
					}}
				>
					{#if item.icon}<item.icon class="size-3.5 shrink-0" />{/if}
					<span class="min-w-0 flex-1 truncate">{item.label}</span>
					{#if item.hint}<span class="text-faint text-xs">{item.hint}</span>{/if}
				</button>
			{/if}
		{/each}
	</div>
{/if}
