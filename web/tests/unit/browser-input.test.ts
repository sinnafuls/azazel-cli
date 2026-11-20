import { describe, expect, it } from 'vitest';
import {
	address,
	Clicks,
	contained,
	fraction,
	keysFor,
	scaleOf,
	Scrolls,
	statusOf,
	Typing,
	wheelDelta,
	type Clock
} from '$lib/browser/input';

/** A clock the test moves by hand. */
function clock() {
	let now = 0;
	let next = 1;
	const timers = new Map<number, { at: number; fn: () => void }>();
	const c: Clock = {
		setTimeout: (fn, ms) => {
			timers.set(next, { at: now + ms, fn });
			return next++;
		},
		clearTimeout: (id) => void timers.delete(id as number)
	};
	const advance = (ms: number) => {
		const until = now + ms;
		for (;;) {
			const due = [...timers].filter(([, t]) => t.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
			if (!due) break;
			timers.delete(due[0]);
			now = due[1].at;
			due[1].fn();
		}
		now = until;
	};
	return { c, advance, now: () => now, waiting: () => timers.size };
}

const FRAME = { width: 1280, height: 720 };

describe('where a point of the screen is in the frame', () => {
	it('is a fraction of the picture when the box has the shape of the frame', () => {
		const box = { left: 100, top: 50, width: 640, height: 360 };
		expect(fraction({ clientX: 100, clientY: 50 }, box, FRAME)).toEqual({ x: 0, y: 0 });
		expect(fraction({ clientX: 420, clientY: 230 }, box, FRAME)).toEqual({ x: 0.5, y: 0.5 });
		expect(fraction({ clientX: 740, clientY: 410 }, box, FRAME)).toEqual({ x: 1, y: 1 });
		expect(fraction({ clientX: 260, clientY: 320 }, box, FRAME)).toEqual({ x: 0.25, y: 0.75 });
	});

	it('leaves out the bars above and below a picture that is wider than its box is', () => {
		// A 16:9 frame in a 400x400 box is drawn 400x225, with a bar of 87.5px above and below.
		const box = { left: 0, top: 0, width: 400, height: 400 };
		expect(contained(box, FRAME)).toEqual({ left: 0, top: 87.5, width: 400, height: 225 });
		expect(fraction({ clientX: 200, clientY: 200 }, box, FRAME)).toEqual({ x: 0.5, y: 0.5 });
		expect(fraction({ clientX: 100, clientY: 87.5 }, box, FRAME)).toEqual({ x: 0.25, y: 0 });
		expect(fraction({ clientX: 400, clientY: 312.5 }, box, FRAME)).toEqual({ x: 1, y: 1 });
		// On a bar: not in the picture.
		expect(fraction({ clientX: 200, clientY: 40 }, box, FRAME)).toBeNull();
		expect(fraction({ clientX: 200, clientY: 360 }, box, FRAME)).toBeNull();
	});

	it('leaves out the bars beside a picture that is taller than its box is', () => {
		// A 720x1280 frame in a 600x400 box is drawn 225x400, with a bar of 187.5px on each side.
		const tall = { width: 720, height: 1280 };
		const box = { left: 20, top: 10, width: 600, height: 400 };
		expect(contained(box, tall)).toEqual({ left: 207.5, top: 10, width: 225, height: 400 });
		expect(fraction({ clientX: 320, clientY: 210 }, box, tall)).toEqual({ x: 0.5, y: 0.5 });
		expect(fraction({ clientX: 207.5, clientY: 110 }, box, tall)).toEqual({ x: 0, y: 0.25 });
		expect(fraction({ clientX: 100, clientY: 210 }, box, tall)).toBeNull();
		expect(fraction({ clientX: 500, clientY: 210 }, box, tall)).toBeNull();
	});

	it('never leaves 0 to 1, and takes a pointer on the very edge as the edge', () => {
		const box = { left: 0, top: 0, width: 640, height: 360 };
		expect(fraction({ clientX: -0.4, clientY: 360.4 }, box, FRAME)).toEqual({ x: 0, y: 1 });
		expect(fraction({ clientX: -2, clientY: 10 }, box, FRAME)).toBeNull();
		expect(fraction({ clientX: 10, clientY: 363 }, box, FRAME)).toBeNull();
	});

	it('is nowhere without a picture or a box', () => {
		const none = { left: 0, top: 0, width: 0, height: 0 };
		expect(fraction({ clientX: 0, clientY: 0 }, none, FRAME)).toBeNull();
		const box = { left: 0, top: 0, width: 10, height: 10 };
		expect(fraction({ clientX: 5, clientY: 5 }, box, { width: 0, height: 0 })).toBeNull();
	});

	it('knows how many pixels of the frame one pixel of the picture is', () => {
		expect(scaleOf({ left: 0, top: 0, width: 640, height: 360 }, FRAME)).toBe(2);
		expect(scaleOf({ left: 0, top: 0, width: 400, height: 400 }, FRAME)).toBe(3.2);
	});
});

describe('keys', () => {
	const key = (key: string, mods: Partial<Parameters<typeof keysFor>[0]> = {}) =>
		keysFor({ key, ...mods });

	it('sends a character as text', () => {
		expect(key('a')).toEqual({ kind: 'text', text: 'a' });
		expect(key('A', { shiftKey: true })).toEqual({ kind: 'text', text: 'A' });
		expect(key('?', { shiftKey: true })).toEqual({ kind: 'text', text: '?' });
		expect(key('é')).toEqual({ kind: 'text', text: 'é' });
		expect(key('😀')).toEqual({ kind: 'text', text: '😀' });
	});

	it('names the keys that are no character, as the browser tool does', () => {
		expect(key('Enter')).toEqual({ kind: 'key', keys: 'Enter' });
		expect(key('Tab', { shiftKey: true })).toEqual({ kind: 'key', keys: 'Shift+Tab' });
		expect(key('ArrowDown', { shiftKey: true })).toEqual({ kind: 'key', keys: 'Shift+ArrowDown' });
		expect(key('Backspace')).toEqual({ kind: 'key', keys: 'Backspace' });
		expect(key(' ')).toEqual({ kind: 'key', keys: 'Space' });
		expect(key('F5')).toEqual({ kind: 'key', keys: 'F5' });
		expect(key('Escape')).toEqual({ kind: 'key', keys: 'Escape' });
	});

	it('sends a chord with its modifiers in front', () => {
		expect(key('a', { ctrlKey: true })).toEqual({ kind: 'key', keys: 'Control+a' });
		expect(key('A', { ctrlKey: true, shiftKey: true })).toEqual({
			kind: 'key',
			keys: 'Control+Shift+a'
		});
		expect(key('c', { metaKey: true })).toEqual({ kind: 'key', keys: 'Meta+c' });
		expect(key('ArrowLeft', { altKey: true })).toEqual({ kind: 'key', keys: 'Alt+ArrowLeft' });
		expect(key('+', { ctrlKey: true })).toEqual({ kind: 'key', keys: 'Control++' });
	});

	it('takes AltGr for what it types, not for Control and Alt', () => {
		expect(
			key('@', { ctrlKey: true, altKey: true, getModifierState: (k) => k === 'AltGraph' })
		).toEqual({ kind: 'text', text: '@' });
	});

	it('sends nothing for a modifier alone, a dead key or text being composed', () => {
		for (const k of ['Shift', 'Control', 'Alt', 'Meta', 'Dead', 'CapsLock', 'AudioVolumeUp', 'F13'])
			expect(key(k), k).toBeNull();
		expect(key('a', { isComposing: true })).toBeNull();
	});
});

describe('the address field', () => {
	it('opens what has a scheme as it is, when it is http or https', () => {
		expect(address('https://example.com/a?b=1')).toBe('https://example.com/a?b=1');
		expect(address('  http://example.com  ')).toBe('http://example.com/');
		expect(address('about:blank')).toBe('about:blank');
		expect(address('file:///etc/passwd')).toBeNull();
		expect(address('javascript://x')).toBeNull();
	});

	it('gives a bare host https, and this machine http', () => {
		expect(address('example.com')).toBe('https://example.com/');
		expect(address('docs.example.com/guide')).toBe('https://docs.example.com/guide');
		expect(address('localhost:5173/app')).toBe('http://localhost:5173/app');
		expect(address('127.0.0.1:8080')).toBe('http://127.0.0.1:8080/');
	});

	it('refuses what is no address', () => {
		for (const t of ['', '   ', 'two words', 'example', 'javascript:alert(1)'])
			expect(address(t), t).toBeNull();
	});
});

describe('the wheel', () => {
	it('is in pixels whatever the device reports', () => {
		expect(wheelDelta({ deltaY: 120, deltaMode: 0 })).toBe(120);
		expect(wheelDelta({ deltaY: 3, deltaMode: 1 })).toBe(120);
		expect(wheelDelta({ deltaY: -1, deltaMode: 2 }, 600)).toBe(-600);
	});

	it('sends the first step at once and adds up those that follow closely', () => {
		const t = clock();
		const sent: [number, number, number][] = [];
		const s = new Scrolls((x, y, dy) => sent.push([x, y, dy]), 100, t.c);
		s.add(0.5, 0.5, 40);
		expect(sent).toEqual([[0.5, 0.5, 40]]);
		// Twelve more steps within the next 100ms are one command, at the place of the last.
		for (let i = 0; i < 12; i++) {
			t.advance(5);
			s.add(0.5, 0.25, 10);
		}
		expect(sent).toHaveLength(1);
		t.advance(100);
		expect(sent).toEqual([
			[0.5, 0.5, 40],
			[0.5, 0.25, 120]
		]);
		// Nothing more is sent when nothing more was scrolled.
		t.advance(1000);
		expect(sent).toHaveLength(2);
		expect(t.waiting()).toBe(0);
		// After a pause the next step goes at once again.
		s.add(0.1, 0.1, -30);
		expect(sent[2]).toEqual([0.1, 0.1, -30]);
	});

	it('sends at most one command every 100ms however fast it turns', () => {
		const t = clock();
		let sent = 0;
		const s = new Scrolls(() => sent++, 100, t.c);
		// One second of turning, a step every 5ms.
		for (let i = 0; i < 200; i++) {
			s.add(0.5, 0.5, 5);
			t.advance(5);
		}
		expect(sent).toBeLessThanOrEqual(11);
		expect(sent).toBeGreaterThanOrEqual(9);
	});

	it('forgets what was not sent when it is cancelled', () => {
		const t = clock();
		const sent: number[] = [];
		const s = new Scrolls((_x, _y, dy) => sent.push(dy), 100, t.c);
		s.add(0, 0, 10);
		s.add(0, 0, 10);
		s.cancel();
		t.advance(500);
		expect(sent).toEqual([10]);
	});
});

describe('clicks', () => {
	const make = () => {
		const t = clock();
		const sent: [number, number, number][] = [];
		/** Which presses the page got, as the host plays what was sent: each by its number in the row. */
		const presses: number[] = [];
		const clicks = new Clicks(
			(x, y, n, continues) => {
				sent.push([x, y, n]);
				for (let press = continues ? n : 1; press <= n; press++) presses.push(press);
			},
			220,
			t.c
		);
		return { t, sent, presses, clicks };
	};

	it('sends a click once no second one followed', () => {
		const { t, sent, clicks } = make();
		clicks.click(0.2, 0.3, 1);
		expect(sent).toEqual([]);
		t.advance(220);
		expect(sent).toEqual([[0.2, 0.3, 1]]);
	});

	it('sends a double click as one command, not as a click and a double click', () => {
		const { t, sent, clicks } = make();
		clicks.click(0.2, 0.3, 1);
		t.advance(90);
		clicks.click(0.2, 0.3, 2);
		t.advance(1000);
		expect(sent).toEqual([[0.2, 0.3, 2]]);
	});

	it('gives the page two presses for a slow double click, not three', () => {
		const { t, sent, presses, clicks } = make();
		clicks.click(0.2, 0.3, 1);
		// The second click comes after the first stopped waiting and went.
		t.advance(300);
		expect(presses).toEqual([1]);
		clicks.click(0.2, 0.3, 2);
		t.advance(1000);
		expect(sent).toEqual([
			[0.2, 0.3, 1],
			[0.2, 0.3, 2]
		]);
		expect(presses).toEqual([1, 2]);
	});

	it('gives the page three presses for a triple click, fast or slow', () => {
		const fast = make();
		fast.clicks.click(0.5, 0.5, 1);
		fast.t.advance(90);
		fast.clicks.click(0.5, 0.5, 2);
		fast.t.advance(90);
		fast.clicks.click(0.5, 0.5, 3);
		fast.t.advance(1000);
		expect(fast.presses).toEqual([1, 2, 3]);

		const slow = make();
		for (const nth of [1, 2, 3]) {
			slow.clicks.click(0.5, 0.5, nth);
			slow.t.advance(300);
		}
		expect(slow.presses).toEqual([1, 2, 3]);
		// A fourth click in the row is one more press, the most the host counts.
		slow.clicks.click(0.5, 0.5, 4);
		expect(slow.presses).toEqual([1, 2, 3, 3]);
	});

	it('gives the page one press for a click sent early because a key followed, then one more', () => {
		const { presses, clicks } = make();
		clicks.click(0.4, 0.4, 1);
		clicks.flush();
		clicks.click(0.4, 0.4, 2);
		expect(presses).toEqual([1, 2]);
	});

	it('sends a double click whole when its first click was forgotten', () => {
		const { t, presses, clicks } = make();
		clicks.click(0.4, 0.4, 1);
		clicks.drop();
		t.advance(300);
		clicks.click(0.4, 0.4, 2);
		expect(presses).toEqual([1, 2]);
	});

	it('sends two clicks in two places as two clicks, in order', () => {
		const { t, sent, clicks } = make();
		clicks.click(0.1, 0.1, 1);
		t.advance(50);
		clicks.click(0.9, 0.9, 1);
		expect(sent).toEqual([[0.1, 0.1, 1]]);
		t.advance(220);
		expect(sent).toEqual([
			[0.1, 0.1, 1],
			[0.9, 0.9, 1]
		]);
	});

	it('sends a click that waits at once when something else follows, and never twice', () => {
		const { t, sent, clicks } = make();
		clicks.click(0.4, 0.4, 1);
		clicks.flush();
		expect(sent).toEqual([[0.4, 0.4, 1]]);
		t.advance(1000);
		expect(sent).toHaveLength(1);
		clicks.click(0.4, 0.4, 1);
		clicks.drop();
		t.advance(1000);
		expect(sent).toHaveLength(1);
	});
});

describe('typing', () => {
	const make = () => {
		const t = clock();
		const sent: object[] = [];
		return { t, sent, typing: new Typing((i) => sent.push(i), 60, 90, t.c, t.now) };
	};

	it('sends characters typed close together as one text', () => {
		const { t, sent, typing } = make();
		for (const ch of 'hello') {
			typing.press({ kind: 'text', text: ch });
			t.advance(10);
		}
		expect(sent).toEqual([]);
		t.advance(60);
		expect(sent).toEqual([{ kind: 'text', text: 'hello' }]);
	});

	it('keeps the order of text and keys', () => {
		const { sent, typing } = make();
		typing.press({ kind: 'text', text: 'a' });
		typing.press({ kind: 'text', text: 'b' });
		typing.press({ kind: 'key', keys: 'Enter' });
		typing.press({ kind: 'text', text: 'c' });
		typing.flush();
		expect(sent).toEqual([
			{ kind: 'text', text: 'ab' },
			{ kind: 'key', keys: 'Enter' },
			{ kind: 'text', text: 'c' }
		]);
	});

	it('repeats a key that is held down at most every 90ms', () => {
		const { t, sent, typing } = make();
		typing.press({ kind: 'key', keys: 'ArrowDown' });
		for (let i = 0; i < 30; i++) {
			t.advance(33);
			typing.press({ kind: 'key', keys: 'ArrowDown' }, true);
		}
		// About a second of holding: the first press and no more than eleven repeats.
		expect(sent.length).toBeGreaterThanOrEqual(10);
		expect(sent.length).toBeLessThanOrEqual(12);
	});

	it('forgets what was not sent when it is cancelled', () => {
		const { t, sent, typing } = make();
		typing.press({ kind: 'text', text: 'secret' });
		typing.cancel();
		t.advance(1000);
		expect(sent).toEqual([]);
	});
});

describe('what the browser is doing', () => {
	it('says so in one of five ways', () => {
		expect(statusOf(null, true)).toBe('none');
		expect(statusOf({ running: false }, true)).toBe('none');
		expect(statusOf({ running: true }, false)).toBe('watching');
		expect(statusOf({ running: true }, true)).toBe('agent');
		expect(statusOf({ running: true, held: true, held_by_you: true }, true)).toBe('you');
		expect(statusOf({ running: true, held: true, held_by_you: false }, false)).toBe('other');
	});
});
