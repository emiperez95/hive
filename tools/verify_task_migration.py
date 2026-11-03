#!/usr/bin/env python3
"""Prove a task-board migration lost nothing.

Compares a pre-migration snapshot (JSON from `reminders list --all`, kept in
~/.hive/cache/task-backups/) with the live Reminders list, reminder by reminder, and reports
every piece of information that is in the snapshot but no longer in the live notes.

    verify_task_migration.py --board thesis   --snapshot <file>
    verify_task_migration.py --board aws-hub  --snapshot <file>
    verify_task_migration.py --selftest

Per reminder it checks, in order:
  native fields   title, priority, due, all-day, created, completed        must be identical
  header fields   every old `key: value`                                    OK       same value under the key
                                                                            RENAMED  same value under the new key
                                                                            MOVED    value found elsewhere in the notes
                                                                            REVIEW   only some of the value survives
                                                                            LOST     value is nowhere in the new notes
  body            the old free text after the `---` fence                   must appear, whitespace-normalised
Allowed on purpose (reported as EXPECTED, not as a problem): `blocked-by: none` dropped, and
`status` moving to `done` on a reminder that was already completed.

Exit status: 1 if anything is LOST or a native field changed; 0 otherwise (REVIEW items are
listed for a human). Read-only: it only reads Reminders.
"""
import argparse
import json
import os
import re
import subprocess
import sys

RENAMES = {
    # Thesis: artefact -> files
    "thesis": {"list": "Thesis", "renames": {"artefact": "files"}},
    # aws-hub: the area/epic swap, applied together
    "aws-hub": {
        "list": "AWS Projects",
        "renames": {"project": "area", "area": "epic", "materials": "files"},
    },
}
KV = re.compile(r"^([A-Za-z][\w-]*): ?(.*)$")
NATIVE = ["title", "priority", "due", "allday", "created", "completed"]
PLACEHOLDERS = {"none", "-", ""}


def parse(notes):
    """(ordered [(key, value)], body text) — the same split hive makes."""
    lines = (notes or "").split("\n")
    header = []
    for line in lines:
        m = KV.match(line)
        if not m:
            break
        header.append((m.group(1), m.group(2).strip()))
    rest = "\n".join(lines[len(header):])
    body = rest.lstrip("\n")
    if body.startswith("---"):
        body = body[3:]
    return header, body.strip()


def norm(s):
    return re.sub(r"\s+", " ", s or "").strip().lower()


def tokens(value):
    return [t for t in re.split(r"[\s,;]+", value) if t]


def check_one(old, new, renames):
    """Findings for one reminder: list of (level, message)."""
    out = []
    for f in NATIVE:
        if old.get(f) != new.get(f):
            out.append(("CHANGED", f"native `{f}`: {old.get(f)!r} -> {new.get(f)!r}"))

    oh, ob = parse(old.get("notes"))
    nh, nb = parse(new.get("notes"))
    new_map = {}
    for k, v in nh:
        if k.lower() in new_map:
            out.append(("REVIEW", f"duplicate key `{k}` in the new header"))
        new_map[k.lower()] = v
    new_text = norm(new.get("notes"))
    completed = bool(old.get("completed"))

    for k, v in oh:
        kl = k.lower()
        target = renames.get(kl, kl)
        have = new_map.get(target)
        if kl == "blocked-by" and norm(v) in PLACEHOLDERS:
            out.append(("EXPECTED", "`blocked-by: none` dropped" if have is None else "`blocked-by: none` kept"))
            continue
        if kl == "status" and have is not None and norm(have) != norm(v):
            if completed and norm(have) == "done":
                out.append(("EXPECTED", f"status {v} -> done (reminder was completed)"))
            else:
                out.append(("REVIEW", f"status changed {v} -> {have}"))
            continue
        if have is not None and norm(have) == norm(v):
            out.append(("RENAMED" if target != kl else "OK", f"`{k}`" + (f" -> `{target}`" if target != kl else "")))
            continue
        if norm(v) and norm(v) in new_text:
            out.append(("MOVED", f"`{k}: {v[:60]}` now elsewhere in the notes"))
            continue
        missing = [t for t in tokens(v) if norm(t) not in new_text]
        if not missing:
            out.append(("MOVED", f"`{k}`: every part of the value is still in the notes, reshaped"))
        elif len(missing) < len(tokens(v)):
            out.append(("REVIEW", f"`{k}: {v[:60]}` — missing from the new notes: {' '.join(missing)[:80]}"))
        else:
            out.append(("LOST", f"`{k}: {v[:80]}` is nowhere in the new notes"))

    if ob:
        if norm(ob) in norm(nb) or norm(ob) in new_text:
            pass
        else:
            old_lines = [l for l in re.split(r"\n+", ob) if l.strip()]
            gone = [l for l in old_lines if norm(l) not in new_text]
            if not gone:
                out.append(("MOVED", "body reflowed or reordered; every line is still there"))
            else:
                level = "LOST" if len(gone) == len(old_lines) else "REVIEW"
                out.append((level, f"body: {len(gone)} of {len(old_lines)} lines missing, first: {gone[0][:70]!r}"))
    return out


def fetch_live(list_name):
    cli = os.environ.get("HIVE_REMINDERS_CMD") or os.path.expanduser(
        "~/.claude/skills/apple-reminders/bin/reminders")
    r = subprocess.run([cli, "list", "--list", list_name, "--all", "--limit", "1000"],
                       capture_output=True, text=True, timeout=60)
    d = json.loads(r.stdout)
    if not d.get("ok"):
        sys.exit(f"Reminders: {d.get('error')}")
    return d["reminders"]


def verify(old_list, new_list, renames, quiet=False):
    new_by_id = {r["id"]: r for r in new_list}
    old_ids = {r["id"] for r in old_list}
    counts, problems, review = {}, 0, 0
    lines = []
    for old in old_list:
        new = new_by_id.get(old["id"])
        title = old["title"][:64]
        if new is None:
            counts["LOST"] = counts.get("LOST", 0) + 1
            problems += 1
            lines.append(f"\n{title}\n    LOST    the whole reminder is gone from the live list")
            continue
        found = check_one(old, new, renames)
        for lvl, _ in found:
            counts[lvl] = counts.get(lvl, 0) + 1
        bad = [f for f in found if f[0] in ("LOST", "CHANGED")]
        rev = [f for f in found if f[0] == "REVIEW"]
        problems += len(bad)
        review += len(rev)
        if bad or rev:
            lines.append(f"\n{title}")
            for lvl, msg in bad + rev:
                lines.append(f"    {lvl:<8}{msg}")
    new_only = [r for r in new_list if r["id"] not in old_ids]
    if not quiet:
        print(f"{len(old_list)} reminders in the snapshot, {len(new_list)} live, {len(new_only)} new since")
        print("  " + "  ".join(f"{k} {v}" for k, v in sorted(counts.items())))
        print("".join(lines) if lines else "\nNothing lost, nothing to review.")
        for r in new_only:
            print(f"\nnew since the snapshot: {r['title'][:70]}")
    return problems, review


def selftest():
    base = {"id": "1", "title": "[X-1] t", "priority": "high", "due": None, "allday": False,
            "created": "c", "completed": False,
            "notes": "status: todo\nproject: ops\narea: onboarding\nmaterials: a.md\nrepo: https://x.dev/r (private)\nblocked-by: none\n---\n## Goal\nline one\nline two"}
    ren = RENAMES["aws-hub"]["renames"]

    def run(**changes):
        new = dict(base)
        new.update(changes)
        return check_one(base, new, ren)

    def levels(f):
        return {l for l, _ in f}

    good = run(notes="status: todo\narea: ops\nepic: onboarding\nfiles: a.md\nrepo: https://x.dev/r\n---\n## Goal\nline one\nline two\n\nRepo (private).")
    assert levels(good) <= {"OK", "RENAMED", "MOVED", "EXPECTED", "REVIEW"}, good
    assert not any(l == "LOST" for l, _ in good), good
    swap = run(notes=base["notes"].replace("project: ops\narea: onboarding", "area: ops\nepic: onboarding"))
    assert sum(1 for l, m in swap if l == "RENAMED") == 2, swap
    # information actually gone
    assert "LOST" in levels(run(notes=base["notes"].replace("materials: a.md\n", ""))), "dropped value not caught"
    assert "LOST" in levels(run(notes=base["notes"].replace("## Goal\nline one\nline two", "## Goal\nrewritten"))) or \
        "REVIEW" in levels(run(notes=base["notes"].replace("line one\nline two", "line one"))), "body loss not caught"
    assert "CHANGED" in levels(run(title="[X-2] t")), "title change not caught"
    assert "CHANGED" in levels(run(priority="low")), "priority change not caught"
    assert "REVIEW" in levels(run(notes=base["notes"].replace("status: todo", "status: doing"))), "status change not flagged"
    done = dict(base, completed=True)
    assert ("EXPECTED", "status todo -> done (reminder was completed)") in check_one(
        done, dict(done, notes=base["notes"].replace("status: todo", "status: done")), ren)
    reflow = run(notes=base["notes"].replace("line one\nline two", "line two\nline one"))
    assert ("MOVED", "body reflowed or reordered; every line is still there") in reflow, reflow
    print("selftest ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--board", choices=sorted(RENAMES))
    ap.add_argument("--snapshot", help="pre-migration JSON from `reminders list --all`")
    ap.add_argument("--rename", action="append", default=[], metavar="OLD=NEW", help="extra rename")
    ap.add_argument("--selftest", action="store_true")
    a = ap.parse_args()
    if a.selftest:
        return selftest()
    if not (a.board and a.snapshot):
        ap.error("--board and --snapshot are required")
    cfg = RENAMES[a.board]
    renames = dict(cfg["renames"])
    renames.update(dict(r.split("=", 1) for r in a.rename))
    old = json.load(open(a.snapshot))["reminders"]
    problems, _ = verify(old, fetch_live(cfg["list"]), renames)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
