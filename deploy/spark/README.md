# Running the model servers on the spark

Small tools for a single inference box (written for a DGX Spark, a 121 GB unified-memory GB10).

- `services.toml`: each model server (engine, model file, port, slots, context per slot) and a memory budget.
- `services.py`: `list`, `plan`, `status`, `apply [--only NAME]`, `stop`. `apply` stops what should not run and starts what should, in tmux, and refuses a set that is over the memory budget. `plan` prints the exact launch commands without running them.
- `tps.py`: tokens per second per stream and in total at 1, 2, 4, 8 and 16 simultaneous requests.
- `memguard.sh`: kills `llama-server` if available memory falls under about 6 GB.
- `bench.py`: runs `sg` against a spec several times and records wall time, tokens, retries and pass or fail.

Edit the paths at the top of `services.py` (`LLAMA_SERVER`, `VLLM`, `TOTAL_GB`) for your machine. Nothing here starts at boot.
