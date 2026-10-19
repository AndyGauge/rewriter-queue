#!/usr/bin/env python3
"""Run the model servers described in services.toml.

  services.py list                  enabled services as JSON (what clients read)
  services.py plan [--only NAME]    commands that would run, and budget checks
  services.py status                what is running and answering
  services.py apply [--only NAME]   stop what should not run, start what should
  services.py stop [NAME]           stop one service (or all)

--only treats NAME as the only enabled service without editing the file.
"""
import argparse
import json
import os
import shlex
import subprocess
import sys
import time
import tomllib
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG = os.environ.get("SERVICES_TOML", os.path.join(HERE, "services.toml"))
LLAMA_SERVER = "/home/andy/llama.cpp/build/bin/llama-server"
VLLM = "/home/andy/vllm-env/bin/vllm"
TOTAL_GB = 121


def load(only=None):
    with open(CONFIG, "rb") as f:
        cfg = tomllib.load(f)
    services = cfg.get("service", [])
    if only:
        if only not in [s["name"] for s in services]:
            sys.exit(f"no service named {only}")
        for s in services:
            s["enabled"] = s["name"] == only
    return cfg.get("budget", {}).get("memory_gb", TOTAL_GB), services


def command(s):
    ctx = s["context_per_slot"]
    if s["engine"] == "llama.cpp":
        return [
            LLAMA_SERVER, "-m", s["model"], "-a", s["served_as"], "-ngl", "999",
            "-c", str(ctx * s["slots"]), "--parallel", str(s["slots"]),
            *s.get("extra", []), "--host", "0.0.0.0", "--port", str(s["port"]),
        ]
    if s["engine"] == "vllm":
        return [
            VLLM, "serve", s["model"], "--served-model-name", s["served_as"],
            "--port", str(s["port"]), "--host", "0.0.0.0",
            "--max-num-seqs", str(s["slots"]), "--max-model-len", str(ctx),
            "--gpu-memory-utilization", f"{s['memory_gb'] / TOTAL_GB:.2f}",
            *s.get("extra", []),
        ]
    sys.exit(f"unknown engine {s['engine']}")


def checks(budget, services):
    problems = []
    enabled = [s for s in services if s.get("enabled")]
    used = sum(s["memory_gb"] for s in enabled)
    if used > budget:
        problems.append(f"enabled services need {used} GB, budget is {budget} GB")
    ports = {}
    for s in enabled:
        if s["port"] in ports:
            problems.append(f"{s['name']} and {ports[s['port']]} both use port {s['port']}")
        ports[s["port"]] = s["name"]
    return problems


def listening_pid(port):
    out = subprocess.run(["ss", "-ltnp", f"sport = :{port}"], capture_output=True, text=True).stdout
    for part in out.split("pid=")[1:]:
        return int(part.split(",")[0].split(")")[0])
    return None


def cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().decode().split("\0")[:-1]
    except OSError:
        return []


def same_command(running, wanted):
    return running[1:] == wanted[1:] and os.path.basename(running[0]) == os.path.basename(wanted[0]) if running else False


def answering(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/v1/models", timeout=3) as r:
            return r.status == 200
    except Exception:
        return False


def session(s):
    return f"svc-{s['name']}"


def stop_port(port):
    pid = listening_pid(port)
    if pid is None:
        return
    subprocess.run(["kill", str(pid)])
    for _ in range(60):
        if listening_pid(port) is None:
            return
        time.sleep(1)
    subprocess.run(["kill", "-9", str(pid)])


def start(s):
    log = f"/home/andy/svc-{s['name']}.log"
    inner = "exec " + " ".join(shlex.quote(a) for a in command(s)) + f" >> {shlex.quote(log)} 2>&1"
    subprocess.run(["tmux", "kill-session", "-t", session(s)], capture_output=True)
    for _ in range(6):
        if subprocess.run(["tmux", "new-session", "-d", "-s", session(s), inner], capture_output=True).returncode == 0:
            break
        time.sleep(2)
    else:
        return False
    for _ in range(600):
        if answering(s["port"]):
            return True
        time.sleep(2)
    return False


def cmd_list(args):
    _, services = load(args.only)
    print(json.dumps([
        {k: s[k] for k in ("name", "engine", "served_as", "port", "slots", "context_per_slot", "memory_gb")}
        for s in services if s.get("enabled")
    ], indent=2))


def cmd_plan(args):
    budget, services = load(args.only)
    for s in services:
        state = "run " if s.get("enabled") else "skip"
        print(f"[{state}] {s['name']}: " + " ".join(shlex.quote(a) for a in command(s)))
    for p in checks(budget, services):
        print("PROBLEM:", p)
    return 1 if checks(budget, services) else 0


def cmd_status(args):
    _, services = load()
    ports = sorted({s["port"] for s in services})
    for port in ports:
        pid = listening_pid(port)
        running = cmdline(pid) if pid else []
        match = [s["name"] for s in services if running and same_command(running, command(s))]
        print(f"port {port}: " + (f"pid {pid} = {match[0] if match else 'unrecognised process'}, "
              f"{'answering' if answering(port) else 'not answering'}" if pid else "nothing listening"))


def cmd_apply(args):
    budget, services = load(args.only)
    problems = checks(budget, services)
    if problems:
        sys.exit("refusing to apply: " + "; ".join(problems))
    wanted = {s["port"]: s for s in services if s.get("enabled")}
    for port in sorted({s["port"] for s in services}):
        pid = listening_pid(port)
        running = cmdline(pid) if pid else []
        if port not in wanted:
            if pid:
                print(f"stopping port {port} (pid {pid})")
                stop_port(port)
            continue
        s = wanted[port]
        if pid and same_command(running, command(s)) and answering(port):
            print(f"{s['name']}: already running")
            continue
        if pid:
            print(f"{s['name']}: replacing pid {pid} on port {port}")
            stop_port(port)
        print(f"{s['name']}: starting")
        if not start(s):
            sys.exit(f"{s['name']} did not answer on port {port}; see /home/andy/svc-{s['name']}.log")
        print(f"{s['name']}: up")


def cmd_stop(args):
    _, services = load()
    for s in services:
        if args.name in (None, s["name"]):
            stop_port(s["port"])
            print(f"stopped port {s['port']}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    for name, fn in (("list", cmd_list), ("plan", cmd_plan), ("status", cmd_status), ("apply", cmd_apply)):
        sp = sub.add_parser(name)
        sp.add_argument("--only")
        sp.set_defaults(fn=fn)
    sp = sub.add_parser("stop")
    sp.add_argument("name", nargs="?")
    sp.set_defaults(fn=cmd_stop)
    args = p.parse_args()
    sys.exit(args.fn(args) or 0)


if __name__ == "__main__":
    main()
