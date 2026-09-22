# rewriter-queue

A resumable job queue for multi-agent LLM code synthesis. You submit a
source tree and a mission; a small organization of agent roles
(implementer, security/maintenance/production-readiness reviewers, a merge
agent for large inputs) iterates on it — patching rather than rewriting
where it can, checkpointing every real step so an interrupted run resumes
instead of starting over — until the reviewers are satisfied or the
iteration budget runs out.

Submit and watch jobs from a terminal, from Zed, or from Claude Code.

Not a hosted service — three small Rust binaries you run yourself, against
whatever OpenAI-compatible inference endpoint you point them at.

Start with the [tutorial](tutorial.md).
