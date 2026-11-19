export function duration(ms: number): string {
	if (ms < 1000) return `${Math.round(ms)} ms`;
	const s = ms / 1000;
	if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
	const m = Math.floor(s / 60);
	return `${m}m ${String(Math.round(s % 60)).padStart(2, '0')}s`;
}

export function tokens(n: number): string {
	if (n < 1000) return String(n);
	if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
	return `${(n / 1_000_000).toFixed(1)}M`;
}

const USD = new Intl.NumberFormat('en-US', {
	style: 'currency',
	currency: 'USD',
	minimumFractionDigits: 2,
	maximumFractionDigits: 2
});

export function cost(usd: number | null | undefined): string {
	if (usd === null || usd === undefined) return '—';
	return usd < 0.01 && usd > 0 ? '<$0.01' : USD.format(usd);
}

export function basename(path: string): string {
	const parts = path.replace(/[\\/]+$/, '').split(/[\\/]/);
	return parts[parts.length - 1] || path;
}

/** `src/lib/a.rs` shortened to its last segments when long. */
export function shortPath(path: string, keep = 3): string {
	const parts = path.split(/[\\/]/).filter(Boolean);
	return parts.length <= keep ? path : `…/${parts.slice(-keep).join('/')}`;
}

export function ago(iso: string, now = Date.now()): string {
	const s = Math.max(0, (now - Date.parse(iso)) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.floor(s / 60)} min ago`;
	if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
	return `${Math.floor(s / 86400)} d ago`;
}

export function resetsIn(unix: number, now = Date.now()): string {
	const s = Math.max(0, unix - now / 1000);
	if (s < 3600) return `${Math.ceil(s / 60)} min`;
	if (s < 86400) return `${Math.round(s / 3600)} h`;
	return `${Math.round(s / 86400)} d`;
}

/** The prompt cache's clock: `4:12` while seconds are left, `cold` after. */
export function cacheLeft(secs: number): string {
	const s = Math.max(0, Math.floor(secs));
	if (s === 0) return 'cold';
	return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}
