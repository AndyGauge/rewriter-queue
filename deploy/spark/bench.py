#!/usr/bin/env python3
"""Run the snippet-graph tool against a spec several times and record how it went.

  bench.py run --label NAME [--runs 3] [--spec commission] [--effort low] [--width N]
  bench.py report

Results append to bench-results.jsonl. A run is "green" when no test fails, the
hand-written checks pass and no function was given up on; clippy is recorded apart.
Runs go one at a time: the point is time to a green result on a quiet server.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "bench-results.jsonl")
DEPS = {"commission": 'rust_decimal = "1"\n', "commission_split": 'rust_decimal = "1"\n'}


def services():
    out = subprocess.run([sys.executable, os.path.join(HERE, "services.py"), "list"], capture_output=True, text=True)
    return json.loads(out.stdout) if out.returncode == 0 else []


def run_once(args, index, svc):
    spec = args.spec
    base = os.path.join(HERE, "cairn")
    work = os.path.join(HERE, "bench", f"{args.label}-{index}")
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(os.path.join(work, "crate", "src"))
    manifest = '[package]\nname = "v2"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n' + DEPS.get(spec, "")
    with open(os.path.join(work, "crate", "Cargo.toml"), "w") as f:
        f.write(manifest)
    open(os.path.join(work, "crate", "src", "lib.rs"), "w").write("\n")
    cmd = [os.path.join(HERE, "bin", "sg"), os.path.join(base, f"{spec}.md"), os.path.join(work, "crate"),
           "--width", str(args.width or (svc[0]["slots"] if svc else 2)),
           "--handoff", os.path.join(work, "handoff")]
    for flag, name in (("--oracle", f"{spec}_oracle.rs"), ("--prelude", f"{spec}_prelude.rs")):
        if os.path.exists(os.path.join(base, name)):
            cmd += [flag, os.path.join(base, name)]
    env = dict(os.environ, PATH=os.path.expanduser("~/.cargo/bin") + ":" + os.environ["PATH"])
    if args.no_review:
        cmd.append("--no-review")
    if args.effort:
        env["SG_REASONING_EFFORT"] = args.effort
    started = time.time()
    with open(os.path.join(work, "out.json"), "w") as out, open(os.path.join(work, "err.log"), "w") as err:
        subprocess.run(cmd, stdout=out, stderr=err, env=env)
    wall = time.time() - started
    text = open(os.path.join(work, "out.json")).read()
    try:
        d = json.loads(text[text.find("{"):])
    except ValueError:
        d = {}
    m = d.get("metrics", {})
    green = d.get("failing_tests") == 0 and bool(d.get("oracle_passed")) and not d.get("failed_nodes")
    row = {
        "label": args.label, "run": index, "spec": spec, "effort": args.effort or "default", "review": not args.no_review,
        "services": svc, "width": args.width or (svc[0]["slots"] if svc else 2),
        "wall_s": round(wall), "green": green, "clippy_clean": d.get("clippy_clean"),
        "failing_tests": d.get("failing_tests"), "oracle_passed": d.get("oracle_passed"),
        "retries": m.get("total_retries"), "rollbacks": sum((d.get("rollbacks") or {}).values()),
        "in_tokens": m.get("input_tokens"), "out_tokens": m.get("output_tokens"),
        "model_calls": sum(1 for l in open(os.path.join(work, "err.log")) if "model call" in l),
        "at": time.strftime("%Y-%m-%d %H:%M:%S"),
    }
    with open(RESULTS, "a") as f:
        f.write(json.dumps(row) + "\n")
    print(json.dumps({k: row[k] for k in ("label", "run", "wall_s", "green", "retries", "out_tokens", "model_calls")}))


def cmd_run(args):
    svc = services()
    for i in range(args.first, args.first + args.runs):
        run_once(args, i, svc)


def cmd_report(_):
    rows = [json.loads(l) for l in open(RESULTS)] if os.path.exists(RESULTS) else []
    labels = []
    for r in rows:
        if r["label"] not in labels:
            labels.append(r["label"])
    print(f"{'label':28} {'runs':>4} {'green':>5} {'wall s (min/med/max)':>22} {'retries med':>11} {'out tokens med':>14}")
    for label in labels:
        rs = [r for r in rows if r["label"] == label]
        walls = [r["wall_s"] for r in rs]
        med = lambda key: statistics.median([r[key] for r in rs if r[key] is not None] or [0])
        print(f"{label:28} {len(rs):>4} {sum(r['green'] for r in rs):>5} "
              f"{min(walls):>6}/{int(statistics.median(walls)):>6}/{max(walls):>6}   {med('retries'):>11} {med('out_tokens'):>14}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--label", required=True)
    r.add_argument("--runs", type=int, default=3)
    r.add_argument("--first", type=int, default=1, help="number of the first run, for alternating labels")
    r.add_argument("--spec", default="commission")
    r.add_argument("--effort", choices=["low", "medium", "high"])
    r.add_argument("--width", type=int)
    r.add_argument("--no-review", action="store_true", help="skip the skeleton review")
    r.set_defaults(fn=cmd_run)
    sub.add_parser("report").set_defaults(fn=cmd_report)
    args = p.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
