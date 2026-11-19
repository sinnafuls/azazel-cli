<script lang="ts">
	// Where a change is kept: this session, every session (home), or this project.
	import * as NativeSelect from '$lib/components/ui/native-select';
	import { Label } from '$lib/components/ui/label';
	import { app } from '$lib/app.svelte';
	import type { SaveTo } from '$lib/protocol';

	let { value = $bindable(), project = true }: { value: SaveTo; project?: boolean } = $props();
	const uid = $props.id();
</script>

<div class="flex shrink-0 items-center gap-2">
	<Label for="{uid}-scope" class="text-xs font-normal text-muted-foreground">Save for</Label>
	<NativeSelect.Root id="{uid}-scope" bind:value aria-label="Where a change is saved">
		<NativeSelect.Option value="session">This session</NativeSelect.Option>
		<NativeSelect.Option value="home" disabled={!app.fullAccess}
			>Every session{app.fullAccess ? '' : ' (needs full access)'}</NativeSelect.Option
		>
		<NativeSelect.Option value="project" disabled={!app.fullAccess || !project}
			>This project{app.fullAccess ? '' : ' (needs full access)'}</NativeSelect.Option
		>
	</NativeSelect.Root>
</div>
