// Images attached to a prompt are downscaled in a canvas so a phone photo does not exceed the
// 12 MiB message limit, and are sent as `{mime, data}` with base64 data.
import type { Image } from '$lib/protocol';

export const MAX_SIDE = 2048;

/** Size after fitting inside MAX_SIDE, keeping the aspect ratio. Never upscales. */
export function fit(w: number, h: number, max = MAX_SIDE): { w: number; h: number } {
	const scale = Math.min(1, max / Math.max(w, h));
	return { w: Math.max(1, Math.round(w * scale)), h: Math.max(1, Math.round(h * scale)) };
}

export async function toImage(file: File): Promise<Image & { preview: string }> {
	const bitmap = await createImageBitmap(file);
	const { w, h } = fit(bitmap.width, bitmap.height);
	const canvas = document.createElement('canvas');
	canvas.width = w;
	canvas.height = h;
	const ctx = canvas.getContext('2d');
	if (!ctx) throw new Error('This browser cannot resize images.');
	ctx.drawImage(bitmap, 0, 0, w, h);
	bitmap.close();
	const keepPng = file.type === 'image/png' && w * h < 1_500_000;
	const mime = keepPng ? 'image/png' : 'image/jpeg';
	const url = canvas.toDataURL(mime, 0.85);
	return { mime, data: url.slice(url.indexOf(',') + 1), preview: url };
}
