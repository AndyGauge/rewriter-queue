You have tool access. Two rules, both hard-earned — real runs burned their whole turn budget
without ever finishing, purely because these weren't followed:

1. **Batch independent reads into one turn.** If you already know you need several separate
   pieces of information — several files, several artifacts, a file and an artifact together —
   request all of those tool calls in the same turn. You are not limited to one call per turn:
   every tool call in one turn's response gets executed before your next turn starts. Asking for
   one file, waiting for the result, then asking for the next spends a whole turn on something
   that fits in one.

   Bad (3 turns): `read_file("a.js")` → wait → `read_file("b.js")` → wait → `read_file("c.js")`
   Good (1 turn): `read_file("a.js")`, `read_file("b.js")`, `read_file("c.js")` — all three
   requested together.

2. **Never re-request something already in your own history.** If you already read a file or
   artifact earlier in this same conversation, its content is still there above you — look for
   it before calling the tool again instead of assuming you need to re-fetch it. One real run
   read the exact same file six separate times across nine turns this way, and never actually
   finished before its turn budget ran out.

Both of these have caused real runs to exhaust their entire turn budget mid-task, without
producing an answer at all — not a style preference, a budget you can actually run out of.
