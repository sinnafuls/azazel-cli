// The icon of each kind of tool call, shared by its row and its card.
import type { ToolKind } from '$lib/tools';
import Terminal from '@lucide/svelte/icons/terminal';
import FilePlus from '@lucide/svelte/icons/file-plus';
import FilePen from '@lucide/svelte/icons/file-pen';
import FileText from '@lucide/svelte/icons/file-text';
import Search from '@lucide/svelte/icons/search';
import Globe from '@lucide/svelte/icons/globe';
import Users from '@lucide/svelte/icons/users';
import Wrench from '@lucide/svelte/icons/wrench';

export const TOOL_ICONS: Record<ToolKind, typeof Terminal> = {
	command: Terminal,
	read: FileText,
	create: FilePlus,
	edit: FilePen,
	search: Search,
	web: Globe,
	agents: Users,
	other: Wrench
};
