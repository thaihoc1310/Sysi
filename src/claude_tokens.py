# Claude Code's token spend on this machine, by local day, for Sysi on
# another machine: run as `python3 - OFFSET [ROOT [CACHE]]` with this file on
# stdin, it prints {"days": {"DAY": [input, output, cache_read, cache_write]}}.
# OFFSET is the seconds the reading machine's clock is ahead of UTC, DAY the
# days since 1970-01-01 on that clock.
#
# The rules are scan_claude_file's and scan_tokens' in src/usage.rs, which a
# test in that file holds this to: lines with "usage" under 256 KiB, the
# usage under message.usage, the day from the first 19 characters of the UTC
# timestamp, and each response counted once by (message.id, requestId) across
# every file, since Claude Code repeats a response per streaming update and
# copies a transcript whole when a session is forked or resumed. Each file's
# records are cached by size and mtime, so only the transcript being written
# is read again.
import calendar
import json
import os
import sys

MAX_RECORD_BYTES = 256 * 1024
SESSION_DEPTH = 3


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


def records(path):
    found, seen = [], set()
    try:
        with open(path, "rb") as transcript:
            for raw in transcript:
                if len(raw) > MAX_RECORD_BYTES or b'"usage"' not in raw:
                    continue
                try:
                    value = json.loads(raw)
                except ValueError:
                    continue
                message = value.get("message") if isinstance(value, dict) else None
                usage = message.get("usage") if isinstance(message, dict) else None
                if usage is None:
                    continue
                when = seconds(value.get("timestamp"))
                if when is None:
                    continue
                key = "%s\0%s" % (message.get("id") or "", value.get("requestId") or "")
                if key in seen:
                    continue
                seen.add(key)
                usage = usage if isinstance(usage, dict) else {}
                found.append([key, when,
                              number(usage.get("input_tokens")),
                              number(usage.get("output_tokens")),
                              number(usage.get("cache_read_input_tokens")),
                              number(usage.get("cache_creation_input_tokens"))])
    except OSError:
        pass
    return found


def transcripts(root, depth):
    try:
        entries = list(os.scandir(root))
    except OSError:
        return
    for entry in entries:
        if entry.is_dir(follow_symlinks=False):
            if depth > 0:
                yield from transcripts(entry.path, depth - 1)
        elif entry.name.endswith(".jsonl"):
            yield entry.path


def main():
    offset = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    root = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.claude/projects")
    cache_path = sys.argv[3] if len(sys.argv) > 3 else os.path.expanduser(
        "~/.cache/sysi-claude-tokens.json")
    try:
        with open(cache_path) as cached:
            cache = json.load(cached)
    except (OSError, ValueError):
        cache = {}
    fresh, days, seen = {}, {}, set()
    for path in transcripts(root, SESSION_DEPTH):
        try:
            stat = os.stat(path)
        except OSError:
            continue
        mark = [stat.st_size, stat.st_mtime_ns]
        entry = cache.get(path)
        if entry is None or entry["mark"] != mark:
            entry = {"mark": mark, "records": records(path)}
        fresh[path] = entry
        for key, when, *spent in entry["records"]:
            if key in seen:
                continue
            seen.add(key)
            total = days.setdefault((when + offset) // 86400, [0, 0, 0, 0])
            for index, amount in enumerate(spent):
                total[index] += amount
    try:
        os.makedirs(os.path.dirname(cache_path), exist_ok=True)
        with open(cache_path + ".tmp", "w") as cached:
            json.dump(fresh, cached)
        os.replace(cache_path + ".tmp", cache_path)
    except OSError:
        pass
    print(json.dumps({"days": {str(day): spent for day, spent in days.items() if sum(spent)}}))


main()
