# Codex's token spend on this machine, by local day, for Sysi on another
# machine: run as `python3 - OFFSET [ROOT [CACHE]]` with this file on stdin, it
# prints {"days": {"DAY": [input, output, cache_read, cache_write]}}. OFFSET is
# the seconds the reading machine's clock is ahead of UTC, DAY the days since
# 1970-01-01 on that clock.
#
# The rules are scan_codex_file's in src/usage.rs, which a test in that file
# holds this to: Codex logs a running session total, so each rollout is read
# as the growth of that total, a jump larger than a turn could spend is a
# fork's inherited counter and is skipped, and the cached prompt is taken back
# out of "input". Each file's days are cached by size and mtime, so only the
# rollout being written is read again.
import calendar
import json
import os
import sys

MAX_RECORD_BYTES = 256 * 1024
SESSION_DEPTH = 3
FIELDS = ("input_tokens", "output_tokens", "cached_input_tokens", "cache_write_input_tokens")


def number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    return int(value) if value > 0 and value != float("inf") else 0


def seconds(stamp):
    if not isinstance(stamp, str) or len(stamp) < 19:
        return None
    if stamp[4] != "-" or stamp[7] != "-" or stamp[10] != "T":
        return None
    try:
        return calendar.timegm(
            (int(stamp[0:4]), int(stamp[5:7]), int(stamp[8:10]),
             int(stamp[11:13]), int(stamp[14:16]), int(stamp[17:19]), 0, 0, 0)
        )
    except ValueError:
        return None


def days_of(path, offset):
    days, previous = {}, [0, 0, 0, 0]
    try:
        with open(path, "rb") as rollout:
            for raw in rollout:
                if len(raw) > MAX_RECORD_BYTES or b"total_token_usage" not in raw:
                    continue
                try:
                    value = json.loads(raw)
                except ValueError:
                    continue
                payload = value.get("payload") if isinstance(value, dict) else None
                info = payload.get("info") if isinstance(payload, dict) else None
                usage = info.get("total_token_usage") if isinstance(info, dict) else None
                if usage is None:
                    continue
                usage = usage if isinstance(usage, dict) else {}
                current = [number(usage.get(field)) for field in FIELDS]
                # A counter that went back started again: all of it is new.
                spent = [now - before if now >= before else now
                         for now, before in zip(current, previous)]
                previous = current
                ceiling = number(info.get("model_context_window")) * 2 or 2_000_000
                if sum(spent) > ceiling:
                    continue
                spent[0] = max(spent[0] - spent[2], 0)
                when = seconds(value.get("timestamp"))
                if when is None:
                    continue
                total = days.setdefault(str((when + offset) // 86400), [0, 0, 0, 0])
                for index, amount in enumerate(spent):
                    total[index] += amount
    except OSError:
        pass
    return days


def rollouts(root, depth):
    try:
        entries = list(os.scandir(root))
    except OSError:
        return
    for entry in entries:
        if entry.is_dir(follow_symlinks=False):
            if depth > 0:
                yield from rollouts(entry.path, depth - 1)
        elif entry.name.endswith(".jsonl"):
            yield entry.path


def main():
    offset = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
    root = sys.argv[2] if len(sys.argv) > 2 else os.path.join(home, "sessions")
    cache_path = sys.argv[3] if len(sys.argv) > 3 else os.path.expanduser(
        "~/.cache/sysi-codex-tokens.json")
    try:
        with open(cache_path) as cached:
            cache = json.load(cached)
    except (OSError, ValueError):
        cache = {}
    fresh, days = {}, {}
    for path in rollouts(root, SESSION_DEPTH):
        try:
            stat = os.stat(path)
        except OSError:
            continue
        mark = [stat.st_size, stat.st_mtime_ns, offset]
        entry = cache.get(path)
        if entry is None or entry["mark"] != mark:
            entry = {"mark": mark, "days": days_of(path, offset)}
        fresh[path] = entry
        for day, spent in entry["days"].items():
            total = days.setdefault(day, [0, 0, 0, 0])
            for index, amount in enumerate(spent):
                total[index] += amount
    try:
        os.makedirs(os.path.dirname(cache_path), exist_ok=True)
        with open(cache_path + ".tmp", "w") as cached:
            json.dump(fresh, cached)
        os.replace(cache_path + ".tmp", cache_path)
    except OSError:
        pass
    print(json.dumps({"days": {day: spent for day, spent in days.items() if sum(spent)}}))


main()
