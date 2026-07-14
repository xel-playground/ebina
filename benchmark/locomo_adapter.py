#!/usr/bin/env python3
"""Converts a slice of the real LOCOMO dataset (snap-research/locomo,
data/locomo10.json) into this benchmark's scenario JSON format.

Parked (see README.md's `locomo_adapter.py` section) — not part of the
core regression suite. Download the dataset first:
    curl -sL "https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json" \\
        -o benchmark/locomo_data/locomo10.json

LOCOMO's schema per conversation:
  conversation: {speaker_a, speaker_b, session_N, session_N_date_time, ...}
    session_N = [{speaker, dia_id (e.g. "D3:7"), text}, ...] — a 2-person
    dialogue, not a single-owner chat with ebina the way this benchmark's
    other scenarios are. ebina has no native "relay a conversation between
    two named third parties" concept, so each session gets folded into one
    seed message: a transcript block prefixed with who's speaking, framed as
    something the owner is sharing with the agent to remember.
  qa: [{question, answer, evidence: ["D1:3", ...], category}, ...]
    category 1=single-hop, 2=temporal, 3=multi-hop, 4=open-domain,
    5=adversarial (has `adversarial_answer` instead of `answer` — the
    *wrong* answer a naive system would give; correct behavior is NOT
    confidently stating it).

This only ever samples a small slice — full LOCOMO (10 conversations, ~200
QA pairs each) run through ebina's real multi-cycle wake pipeline would be
enormous real API spend for what's still an experimental harness. See
README.md's cost note.

Usage:
    python3 benchmark/locomo_adapter.py --conversation 0 --sessions 4 --max-questions 20
"""
import argparse
import json
import os
import re

REPO_ROOT = os.path.dirname(os.path.abspath(__file__))
DATASET_PATH = os.path.join(REPO_ROOT, "locomo_data/locomo10.json")
CATEGORY_NAMES = {1: "single-hop", 2: "temporal", 3: "multi-hop", 4: "open-domain", 5: "adversarial"}


def session_to_seed(conversation, session_key, date_key):
    turns = conversation[session_key]
    date = conversation.get(date_key, "")
    lines = [f'{t["speaker"]}: {t["text"]}' for t in turns]
    transcript = "\n".join(lines)
    return (
        f"Here's a conversation I had on {date} — please remember anything worth "
        f"keeping from it:\n\n{transcript}"
    )


def evidence_session_numbers(evidence):
    """['D3:7', 'D1:2'] -> {3, 1}"""
    out = set()
    for e in evidence:
        m = re.match(r"D(\d+):", e)
        if m:
            out.add(int(m.group(1)))
    return out


def build_scenario(conv, n_sessions, max_questions, name):
    conversation = conv["conversation"]
    days = []
    for i in range(1, n_sessions + 1):
        key, date_key = f"session_{i}", f"session_{i}_date_time"
        if key not in conversation:
            break
        days.append({"seed": [session_to_seed(conversation, key, date_key)]})

    # Only QA pairs whose evidence is entirely within the sessions actually
    # seeded — a question whose evidence lives in session 7 has no business
    # being asked if we only fed sessions 1-4, it would just be an unfair
    # adversarial-by-accident question.
    in_range_qa = [
        q for q in conv["qa"] if evidence_session_numbers(q.get("evidence", [])) <= set(range(1, n_sessions + 1))
    ]

    # Spread the sample across categories rather than taking the first N
    # (which could accidentally be all one category).
    by_category = {}
    for q in in_range_qa:
        by_category.setdefault(q["category"], []).append(q)
    sampled = []
    per_category_cap = max(1, max_questions // max(1, len(by_category)))
    for cat, qs in by_category.items():
        sampled.extend(qs[:per_category_cap])
    sampled = sampled[:max_questions]

    checks = []
    for i, q in enumerate(sampled):
        cat = q["category"]
        cid = f"locomo_{cat}_{i}"
        if cat == 5:
            # adversarial: the trap is `adversarial_answer`, not a real fact
            # to recall — correct behavior is not confidently stating it.
            checks.append(
                {
                    "id": cid,
                    "type": "ask",
                    "question": q["question"],
                    "expect_not_contains": q["adversarial_answer"],
                    "_category": CATEGORY_NAMES[cat],
                    "_locomo_evidence": q.get("evidence"),
                }
            )
        else:
            checks.append(
                {
                    "id": cid,
                    "type": "ask",
                    "question": q["question"],
                    "expect_contains": str(q["answer"]),
                    "_category": CATEGORY_NAMES[cat],
                    "_locomo_evidence": q.get("evidence"),
                }
            )

    return {
        "name": name,
        "description": (
            f"Real LOCOMO data (snap-research/locomo, sample_id={conv.get('sample_id')}), "
            f"{len(days)} sessions, {len(checks)} questions across {len(by_category)} categories "
            f"({', '.join(sorted(set(c['_category'] for c in checks)))})."
        ),
        "days": days,
        "run_summary_after": True,
        "checks": checks,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--conversation", type=int, default=0, help="index into locomo10.json (0-9)")
    ap.add_argument("--sessions", type=int, default=4, help="how many sessions to seed")
    ap.add_argument("--max-questions", type=int, default=20)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    dataset = json.load(open(DATASET_PATH))
    conv = dataset[args.conversation]
    name = f"locomo_conv{args.conversation}_s{args.sessions}"
    scenario = build_scenario(conv, args.sessions, args.max_questions, name)

    # NOT scenarios/ — that dir is auto-globbed by a bare `run_benchmark.py`
    # (no args) as the core correctness-verification suite; a LOCOMO sample
    # landing there would silently get pulled into every future run and
    # its real API cost with it. Generated samples stay opt-in only.
    out_path = args.out or os.path.join(REPO_ROOT, "locomo_data/generated_scenarios", f"{name}.json")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    json.dump(scenario, open(out_path, "w"), ensure_ascii=False, indent=2)
    print(f"wrote {out_path}")
    print(f"  {len(scenario['days'])} sessions seeded")
    print(f"  {len(scenario['checks'])} questions:")
    for c in scenario["checks"]:
        print(f"    [{c['_category']}] {c['question']}")


if __name__ == "__main__":
    main()
