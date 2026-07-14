#!/usr/bin/env python3
"""Runs one or more benchmark scenarios against a fresh, isolated testbed
agent-home (never touches tmp/main). See README.md for the design.

Usage:
    python3 benchmark/run_benchmark.py                    # all scenarios
    python3 benchmark/run_benchmark.py single_hop          # one scenario
    python3 benchmark/run_benchmark.py single_hop tier_accuracy
"""
import glob
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib

REPO_ROOT = lib.REPO_ROOT
SCENARIOS_DIR = os.path.join(REPO_ROOT, "benchmark/scenarios")
RESULTS_DIR = os.path.join(REPO_ROOT, "benchmark/results")

UNCERTAIN_PHRASES = [
    "不知道", "不清楚", "沒有印象", "沒有記錄", "沒有提過", "沒提到", "沒有相關",
    "無法確定", "不確定", "沒有這個資訊", "沒看到", "找不到", "沒有找到",
    "don't know", "no record", "not mentioned", "no information", "not sure",
]


def reset_testbed():
    subprocess.run(["bash", os.path.join(REPO_ROOT, "benchmark/setup_testbed.sh")], check=True, cwd=REPO_ROOT)


def run_scenario(scenario_path):
    scenario = json.load(open(scenario_path))
    name = scenario["name"]
    print(f"\n=== {name} ===")
    print(scenario.get("description", ""))

    # Real cadence ratios: 6 daily_maintenance passes per maintenance_summary,
    # 4 maintenance_summary passes per core_distillation (adr/001-memory-v2.md
    # §1/§2.3 — "要活過 4 份 6h summary 才有資格競爭 core 席位"). Configurable
    # per scenario since simulating the full ratio is expensive (real LLM
    # calls each time) — a scenario can override to test faster with fewer
    # days, or set these to exercise the *actual* "must survive N cycles"
    # promotion gate for real.
    summary_every_n_days = scenario.get("summary_every_n_days", len(scenario["days"]))
    core_after_n_summaries = scenario.get("core_after_n_summaries", 1)

    reset_testbed()
    with lib.Testbed():
        summaries_run = 0
        for i, day in enumerate(scenario["days"]):
            print(f"  day {i}: seeding {len(day['seed'])} message(s)...")
            for msg in day["seed"]:
                result = lib.seed_message(msg)
                if not result.get("ok", True):
                    print(f"    ! seed failed: {result}")
            since = lib.last_maintenance_ts()
            print(f"  day {i}: daily_maintenance (since_ts={since})...")
            lib.fire_trigger("daily_maintenance", since)

            is_last_day = i == len(scenario["days"]) - 1
            due_for_summary = (i + 1) % summary_every_n_days == 0
            if scenario.get("run_summary_after", True) and (due_for_summary or is_last_day):
                since = lib.last_summary_ts()
                print(f"  day {i}: maintenance_summary (since_ts={since})...")
                lib.fire_trigger("maintenance_summary", since)
                summaries_run += 1

                due_for_core = summaries_run % core_after_n_summaries == 0
                if due_for_core or (is_last_day and scenario.get("force_core_at_end", True)):
                    since = lib.last_core_ts()
                    print(f"  day {i}: core_distillation (since_ts={since}, after {summaries_run} summary pass(es))...")
                    lib.fire_trigger("core_distillation", since)

        print("  running checks...")
        results = [run_check(c) for c in scenario["checks"]]

    passed = sum(1 for r in results if r["passed"])
    print(f"  {passed}/{len(results)} checks passed")
    for r in results:
        mark = "PASS" if r["passed"] else "FAIL"
        print(f"    [{mark}] {r['id']}")
        if not r["passed"]:
            print(f"           {r['detail']}")

    return {"scenario": name, "results": results, "passed": passed, "total": len(results)}


def _contains(haystack, needle):
    """Case-insensitive substring check — an LLM writing 'Rent' vs a
    scenario's 'rent' isn't a memory-system failure, it's a test harness
    being needlessly strict. Every keyword-matching check goes through
    this, not just some of them."""
    return needle.lower() in haystack.lower()


def run_check(check):
    ctype = check["type"]
    cid = check["id"]

    if ctype == "core_includes":
        text = lib.read_core_md()
        ok = _contains(text, check["keyword"])
        return {"id": cid, "passed": ok, "detail": f"core.md {'contains' if ok else 'missing'} {check['keyword']!r}"}

    if ctype == "core_includes_any":
        text = lib.read_core_md()
        hit = [k for k in check["keywords"] if _contains(text, k)]
        ok = len(hit) > 0
        return {"id": cid, "passed": ok, "detail": f"core.md contains {hit}" if ok else f"core.md has none of {check['keywords']} — LLM may have phrased it differently"}

    if ctype == "core_excludes":
        text = lib.read_core_md()
        ok = not _contains(text, check["keyword"])
        return {"id": cid, "passed": ok, "detail": f"core.md {'correctly excludes' if ok else 'wrongly contains'} {check['keyword']!r}"}

    if ctype == "notes_includes":
        notes = lib.read_all_notes()
        found = [f for f, c in notes.items() if _contains(c, check["keyword"])]
        ok = len(found) > 0
        return {"id": cid, "passed": ok, "detail": f"found in {found}" if ok else f"{check['keyword']!r} not in any notes file ({list(notes.keys())})"}

    if ctype == "notes_excludes":
        notes = lib.read_all_notes()
        found = [f for f, c in notes.items() if _contains(c, check["keyword"])]
        ok = len(found) == 0
        return {"id": cid, "passed": ok, "detail": "correctly absent" if ok else f"wrongly found in {found}"}

    if ctype == "workspace_includes":
        ws = lib.read_workspace_memory()
        found = [f for f, c in ws.items() if _contains(c, check["keyword"])]
        ok = len(found) > 0
        return {"id": cid, "passed": ok, "detail": f"found in {found}" if ok else f"{check['keyword']!r} not in workspace/memory/ ({list(ws.keys())})"}

    if ctype == "ask":
        answer = lib.ask(check["question"])
        ok = True
        detail = []
        if "expect_contains" in check:
            hit = _contains(answer, check["expect_contains"])
            ok = ok and hit
            detail.append(f"expected {check['expect_contains']!r}: {'yes' if hit else 'NO'}")
        if "expect_not_contains" in check:
            miss = not _contains(answer, check["expect_not_contains"])
            ok = ok and miss
            detail.append(f"should not contain {check['expect_not_contains']!r}: {'correct' if miss else 'PRESENT'}")
        return {"id": cid, "passed": ok, "detail": f"answer={answer!r} | {'; '.join(detail)}"}

    if ctype == "ask_uncertain":
        answer = lib.ask(check["question"])
        ok = any(_contains(answer, p) for p in UNCERTAIN_PHRASES)
        return {"id": cid, "passed": ok, "detail": f"answer={answer!r} | {'expressed uncertainty' if ok else 'NO uncertainty phrase found — possible hallucination'}"}

    raise ValueError(f"unknown check type: {ctype}")


def main():
    requested = sys.argv[1:]
    all_scenarios = sorted(glob.glob(os.path.join(SCENARIOS_DIR, "*.json")))
    if requested:
        all_scenarios = [p for p in all_scenarios if os.path.splitext(os.path.basename(p))[0] in requested]

    os.makedirs(RESULTS_DIR, exist_ok=True)
    run_results = [run_scenario(p) for p in all_scenarios]

    total_passed = sum(r["passed"] for r in run_results)
    total_checks = sum(r["total"] for r in run_results)
    print(f"\n=== TOTAL: {total_passed}/{total_checks} checks passed across {len(run_results)} scenario(s) ===")

    out_path = os.path.join(RESULTS_DIR, f"{int(time.time())}.json")
    json.dump(run_results, open(out_path, "w"), ensure_ascii=False, indent=2)
    print(f"full results: {out_path}")


if __name__ == "__main__":
    main()
