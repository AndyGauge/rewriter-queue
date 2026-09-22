# rewriter-queue (Claude Code plugin)

Packages two things for Claude Code:

- **The `rewriter-queue` MCP server** — `queue_submit`, `queue_list`,
  `queue_status`, `queue_cancel`, `queue_fetch`, `queue_artifacts`,
  `queue_artifact`, `queue_download`. Same tools the Zed extension gets;
  just ask in plain language ("what jobs are running?", "download job 4's
  artifacts") and Claude Code calls the right one.
- **Every agent persona from `../skills/` as a slash command** —
  `/rewriter-queue:security-reviewer`, `/rewriter-queue:production-readiness-reviewer`,
  `/rewriter-queue:mission-architect`, etc. Each one is the exact persona
  and verdict format the full pipeline uses for that role, usable
  standalone for a single review pass against whatever you point it at —
  no need to run a whole synthesis job just to get one opinion.

## Install

1. **Build and install the binary** the MCP server config expects on `PATH`:

   ```sh
   cargo install --path ../job-queue
   ```

   (Or point the MCP config at an absolute path instead — edit
   `mcp-servers/config.json`.)

2. **Point it at a queue.** Same config as everywhere else in this repo —
   `~/.config/rewriter/config.toml` with a `[queue] url` (or nothing, to
   fall back to a local file-backed queue) and
   `~/.config/rewriter/queue-token`.

3. **Add and install the plugin**, from the parent directory of this repo:

   ```
   /plugin marketplace add ./rewriter-queue/claude-code-plugin
   /plugin install rewriter-queue@rewriter-queue-marketplace
   ```

   If Claude Code says to run `/reload-plugins`, do that.

## Using the review personas standalone

Each command's body is a straight copy of its skill file in `../skills/`
(skills are the source of truth — if you edit one there, re-copy it here,
or symlink `commands/` to `../skills/` instead of copying if you want them
to always stay in sync). Pass what to review as an argument, or just ask
with something already open:

```
/rewriter-queue:production-readiness-reviewer src/main.rs
```

The persona will still respond in its pipeline verdict format
(`READY`/`NOT_READY`, `APPROVE`/`REJECT`, `SOUND`/`UNSOUND`, ...) since
that's part of what makes each one a precise, single-purpose check rather
than a generic "review this" prompt.
