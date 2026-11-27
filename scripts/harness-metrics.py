#!/usr/bin/env python3
"""Summarize t3d run efficiency from its SQLite store (stdlib only)."""
import argparse
import datetime as dt
import json
import os
import sqlite3
import sys


def decoded(value):
    try:
        return json.loads(value)
    except (TypeError, json.JSONDecodeError):
        return value


def parse_time(value):
    if not value:
        return None
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def percentile(values, p):
    if not values:
        return None
    xs = sorted(values)
    pos = (len(xs) - 1) * p
    lo = int(pos)
    hi = min(lo + 1, len(xs) - 1)
    return round(xs[lo] + (xs[hi] - xs[lo]) * (pos - lo), 2)


def summarize(rows):
    count = len(rows)
    completed = [r for r in rows if decoded(r["status"]) == "completed"]
    durations = [r["duration_ms"] for r in rows if r["duration_ms"] is not None]
    token_total = sum((r["input_tokens"] or 0) + (r["output_tokens"] or 0) + (r["reasoning_tokens"] or 0) for r in rows)
    wasted = sum((r["input_tokens"] or 0) + (r["output_tokens"] or 0) + (r["reasoning_tokens"] or 0) for r in rows if decoded(r["status"]) != "completed")
    return {
        "runs": count,
        "completed_runs": len(completed),
        "completion_rate": len(completed) / count if count else 0,
        "wasted_tokens": wasted,
        "total_tokens": token_total,
        "wasted_token_share": wasted / token_total if token_total else 0,
        "duration_ms_p50": percentile(durations, .5),
        "duration_ms_p90": percentile(durations, .9),
        "by_provider": group(rows, lambda r: str(decoded(r["provider"]))),
        "by_role": group(rows, lambda r: r["role"]),
        "top_errors": top_errors(rows),
    }


def group(rows, key_fn):
    result = {}
    for key in sorted({key_fn(r) for r in rows}):
        result[key] = summarize_counts([r for r in rows if key_fn(r) == key])
    return result


def summarize_counts(rows):
    n = len(rows)
    done = sum(decoded(r["status"]) == "completed" for r in rows)
    tokens = lambda r: (r["input_tokens"] or 0) + (r["output_tokens"] or 0) + (r["reasoning_tokens"] or 0)
    total = sum(tokens(r) for r in rows)
    wasted = sum(tokens(r) for r in rows if decoded(r["status"]) != "completed")
    return {"runs": n, "completed_runs": done, "completion_rate": done / n if n else 0,
            "wasted_tokens": wasted, "total_tokens": total,
            "wasted_token_share": wasted / total if total else 0}


def top_errors(rows):
    counts = {}
    for r in rows:
        if decoded(r["status"]) != "completed" and r["error_message"]:
            counts[r["error_message"]] = counts.get(r["error_message"], 0) + 1
    return [{"message": msg, "count": n} for msg, n in sorted(counts.items(), key=lambda x: (-x[1], x[0]))[:10]]


def load_rows(db, since=None, until=None):
    uri = "file:" + os.path.abspath(os.path.expanduser(db)) + "?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    conn.row_factory = sqlite3.Row
    try:
        query = """SELECT r.*, t.parent_thread_id, t.relationship,
          CASE WHEN b.coordinator_thread_id=t.id THEN 'swarm:coordinator'
               WHEN m.thread_id IS NOT NULL THEN 'swarm:worker'
               WHEN t.relationship IN ('delegated','delegation') THEN 'delegated:child'
               ELSE 'direct:user' END AS role
          FROM runs r JOIN threads t ON t.id=r.thread_id
          LEFT JOIN swarm_boards b ON b.coordinator_thread_id=t.id
          LEFT JOIN swarm_members m ON m.thread_id=t.id"""
        conditions, params = [], []
        if since:
            conditions.append("r.started_at >= ?"); params.append(since)
        if until:
            conditions.append("r.started_at < ?"); params.append(until)
        if conditions:
            query += " WHERE " + " AND ".join(conditions)
        return [dict(row) for row in conn.execute(query, params)]
    finally:
        conn.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", default="~/.t3-rs/state.sqlite")
    parser.add_argument("--since", help="inclusive ISO timestamp")
    parser.add_argument("--until", help="exclusive ISO timestamp")
    parser.add_argument("--compare-before", help="ISO timestamp dividing before/after periods")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    try:
        parse_time(args.since); parse_time(args.until); parse_time(args.compare_before)
        rows = load_rows(args.db, args.since, args.until)
        report = summarize(rows)
        if args.compare_before:
            pivot = parse_time(args.compare_before)
            before = [r for r in rows if parse_time(r["started_at"]) and parse_time(r["started_at"]) < pivot]
            after = [r for r in rows if parse_time(r["started_at"]) and parse_time(r["started_at"]) >= pivot]
            report["comparison"] = {"before": summarize(before), "after": summarize(after)}
        if args.json:
            print(json.dumps(report, indent=2, ensure_ascii=False))
        else:
            print(f"Runs: {report['runs']} | completion: {report['completion_rate']:.1%} | wasted tokens: {report['wasted_token_share']:.1%}")
            print(f"Duration p50/p90: {report['duration_ms_p50']} / {report['duration_ms_p90']} ms")
            print("By provider:", json.dumps(report["by_provider"], ensure_ascii=False))
            print("By role:", json.dumps(report["by_role"], ensure_ascii=False))
            print("Top errors:", json.dumps(report["top_errors"], ensure_ascii=False))
            if "comparison" in report:
                print("Comparison:", json.dumps(report["comparison"], ensure_ascii=False))
        return 0
    except (OSError, sqlite3.Error, ValueError) as exc:
        print(f"harness-metrics: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
