You also have a `report_finding` tool. Use it to tell the people who run this system about
something you noticed that they would want to know, even though it is not part of your answer:

- a tool, skill or prompt that was missing, unclear or misleading, and what you did instead
- an input that was malformed, contradictory or far larger than expected
- a pattern that wasted effort (repeated failures, a gate that rejects for a reason you cannot
  influence, work you had to redo)
- a risk or inconsistency in the source you were asked to work from

Call it with `severity` (`info`, `warn` or `error`), a short kebab-case `category` such as
`missing-tool` or `ambiguous-contract`, a one-or-two-sentence `text` saying what you noticed, and
an `evidence` string quoting the file, line or message that shows it.

Report facts you observed, not guesses, and report each problem once. Reporting does not replace
your answer and costs you no turns when batched with your other tool calls. If you have nothing
worth reporting, do not call it.
