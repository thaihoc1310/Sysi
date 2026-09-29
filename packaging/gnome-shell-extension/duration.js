// What the timer's field accepts, and how the time is written. Kept free of
// GNOME imports so `scripts/build-deb.sh` can run the checks at the bottom
// with plain Node.

// `25`, `25m`, `90s`, `1h30`, `1h30m`, `10:00` (m:ss), `1:30:00`, `@17:30`.
// Seconds, or null for anything else.
export function parseDuration(raw, now = new Date()) {
    const text = String(raw).trim().toLowerCase().replace(/\s+/g, '');
    let m;
    if ((m = text.match(/^@(\d{1,2})[:h](\d{2})$/))) {
        if (+m[1] > 23 || +m[2] > 59)
            return null;
        const at = new Date(now);
        at.setHours(+m[1], +m[2], 0, 0);
        if (at <= now)
            at.setDate(at.getDate() + 1);
        return Math.round((at - now) / 1000);
    }
    if ((m = text.match(/^(\d+):(\d{2}):(\d{2})$/)))
        return +m[1] * 3600 + +m[2] * 60 + +m[3];
    if ((m = text.match(/^(\d+):(\d{2})$/)))
        return +m[1] * 60 + +m[2];
    if ((m = text.match(/^(\d+)$/)))
        return +m[1] * 60;
    if ((m = text.match(/^(\d+)h(\d+)$/)))
        return +m[1] * 3600 + +m[2] * 60;
    if ((m = text.match(/^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/)) && (m[1] || m[2] || m[3]))
        return (m[1] ? +m[1] * 3600 : 0) + (m[2] ? +m[2] * 60 : 0) + (m[3] ? +m[3] : 0);
    return null;
}

// `14:59`, or `1:05:00` past an hour.
export function formatTime(seconds) {
    const s = Math.max(0, Math.ceil(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor(s % 3600 / 60);
    const pad = n => String(n).padStart(2, '0');
    return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

// Run by `node duration.js`: a few inputs of each form.
export function check() {
    const at = new Date(2026, 0, 1, 17, 0, 0);
    const cases = [
        ['25', 1500], ['25m', 1500], ['90s', 90], ['1h30', 5400], ['1h30m', 5400],
        ['1h 30m', 5400], ['2h', 7200], ['10:00', 600], ['1:30:00', 5400],
        ['@17:30', 1800], ['@16:59', 86340], ['@25:00', null], ['abc', null], ['', null],
    ];
    for (const [input, want] of cases) {
        const got = parseDuration(input, at);
        if (got !== want)
            throw new Error(`parseDuration(${JSON.stringify(input)}) = ${got}, want ${want}`);
    }
    for (const [seconds, want] of [[59.2, '1:00'], [600, '10:00'], [3725, '1:02:05'], [-3, '0:00']]) {
        if (formatTime(seconds) !== want)
            throw new Error(`formatTime(${seconds}) = ${formatTime(seconds)}, want ${want}`);
    }
}
