"""Recurring job scheduling from cron-style expressions."""

import re

FIELD_NAMES = ("minute", "hour", "day_of_month", "month", "day_of_week")
FIELD_RANGES = ((0, 59), (0, 23), (1, 31), (1, 12), (0, 6))
STEP_RE = re.compile(r"^(?P<base>[^/]+)(?:/(?P<step>\d+))?$")


def parse_cron_expression(expr):
    """Expand a five-field cron expression into per-field allowed value sets.

    Supports `*`, `a-b` ranges, `a,b,c` lists, and `*/n` or `a-b/n` steps.
    Raises ValueError on anything else rather than silently scheduling wrong.
    """
    fields = expr.split()
    if len(fields) != len(FIELD_NAMES):
        raise ValueError(f"expected {len(FIELD_NAMES)} fields, got {len(fields)}")
    return {
        name: _expand_field(field, lo, hi)
        for name, field, (lo, hi) in zip(FIELD_NAMES, fields, FIELD_RANGES)
    }


def _expand_field(field, lo, hi):
    allowed = set()
    for part in field.split(","):
        match = STEP_RE.match(part)
        if match is None:
            raise ValueError(f"malformed cron field: {part!r}")
        step = int(match.group("step") or 1)
        if step < 1:
            raise ValueError("step must be positive")
        base = match.group("base")
        if base == "*":
            start, end = lo, hi
        elif "-" in base:
            start_s, end_s = base.split("-", 1)
            start, end = int(start_s), int(end_s)
        else:
            start = end = int(base)
        if start < lo or end > hi or start > end:
            raise ValueError(f"cron field {part!r} out of range {lo}-{hi}")
        allowed.update(range(start, end + 1, step))
    return allowed


def matches(schedule, moment):
    """True when `moment` (a datetime) satisfies every field of the schedule."""
    return (
        moment.minute in schedule["minute"]
        and moment.hour in schedule["hour"]
        and moment.day in schedule["day_of_month"]
        and moment.month in schedule["month"]
        and moment.weekday() in schedule["day_of_week"]
    )


def next_run_after(schedule, moment, limit_minutes=60 * 24 * 366):
    """Walk forward minute by minute to the next matching moment.

    Deliberately naive: correctness over cleverness, and the caller only ever
    asks about the next occurrence, which is nearly always minutes away.
    """
    import datetime

    cursor = moment.replace(second=0, microsecond=0)
    for _ in range(limit_minutes):
        cursor += datetime.timedelta(minutes=1)
        if matches(schedule, cursor):
            return cursor
    return None
