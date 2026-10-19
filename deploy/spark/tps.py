#!/usr/bin/env python3
"""Measure generation speed of whatever answers on a port.

  tps.py [--port 8001] [--max-tokens 400] [--levels 1,2,4,8]

For each level, sends that many identical requests at once and reports tokens per
second per stream and in total. Tokens are the server's own completion count, which
includes any reasoning tokens, so it is the work the server actually did.
"""
import argparse
import concurrent.futures
import json
import time
import urllib.request

PROMPT = (
    "Write five small, independent Rust functions with full bodies, each with a one-line doc "
    "comment: sum of a slice of i64, maximum of a slice of f64 (None when empty), reverse the "
    "words in a &str, count vowels in a &str, and check whether a u64 is prime. Output only code."
)


def call(port, model, max_tokens):
    body = {"model": model, "max_tokens": max_tokens, "temperature": 0.2,
            "messages": [{"role": "user", "content": PROMPT}]}
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions",
                                 json.dumps(body).encode(), {"Content-Type": "application/json"})
    started = time.time()
    r = json.load(urllib.request.urlopen(req, timeout=900))
    return r["usage"]["completion_tokens"], r["usage"].get("prompt_tokens", 0), time.time() - started


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8001)
    p.add_argument("--max-tokens", type=int, default=400)
    p.add_argument("--levels", default="1,2,4,8")
    a = p.parse_args()
    models = json.load(urllib.request.urlopen(f"http://127.0.0.1:{a.port}/v1/models", timeout=10))
    model = models["data"][0]["id"]
    call(a.port, model, 16)
    print(f"model {model}")
    print(f"{'streams':>7} {'tok/s per stream':>17} {'tok/s total':>12} {'wall s':>7}")
    for n in [int(x) for x in a.levels.split(",")]:
        started = time.time()
        with concurrent.futures.ThreadPoolExecutor(n) as ex:
            results = list(ex.map(lambda _: call(a.port, model, a.max_tokens), range(n)))
        wall = time.time() - started
        tokens = sum(r[0] for r in results)
        per = sum(r[0] / r[2] for r in results) / n
        print(f"{n:>7} {per:>17.1f} {tokens / wall:>12.1f} {wall:>7.1f}")


if __name__ == "__main__":
    main()
