# tidy

Reads UTF-8 text on standard input and prints it tidied up:

- blank (empty or whitespace-only) lines are dropped
- the remaining lines are sorted in ascending byte order
- duplicate lines are removed
- each line is indented with four spaces
