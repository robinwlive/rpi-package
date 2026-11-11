# rpi-hashline

`hash_read` and `hash_edit`: read with **line numbers**, edit **by those numbers**
instead of retyping text. A whole-file snapshot tag guards freshness.

These are **additional** tools. The built-in `read`, `edit` and `write` stay
installed and untouched — the model picks per task.

## Why not replace the built-ins

Shadowing `read`/`edit` was the first design and it is wrong, for three measured
reasons:

1. **Reading is the common case and hashline makes reading cost more.** On a
   50-line file, the built-in `read` returns 1041 bytes and `hash_read` 1207 —
   +16%, paid on every read. Read-then-analyze work (a knowledge base, docs,
   research) never edits, so it only ever pays.
2. **A single-line edit is not cheaper.** Changing one line needs
   `PUT 25.=25:\n+…` (28 bytes) versus the built-in's `oldText` of the line being
   changed (20 bytes here): the hunk header dominates. The win appears when a
   block of lines changes or when indentation would have to be reproduced
   exactly.
3. **The built-in `edit` has options this cannot express.** `replaceAll` changes
   every occurrence at once; line-number addressing has no equivalent, so
   shadowing the built-in silently removes a capability.

So: same directory, four tools, no collision. Use the built-ins by default and
switch to `hash_*` when the edit is structural — many lines, a range, or text
whose exact whitespace would be painful to reproduce.

## What the model sees

`hash_read` prints a header with a 4-hex snapshot tag, then numbered lines:

```text
src/parser.rs #DE7C - 3 lines
1:fn main() {
2:    run();
3:}
```

`hash_edit` takes hunks addressed with those numbers:

```text
PUT 2.=2:            replace line 2
+    run_fast();

PUT >9:              insert after line 9   (`<5` before line 5, `>$` end of file)
+// note

CUT 4.=6             delete lines 4 through 6
```

Pass the tag back as `tag` (with or without `#`) and a file that changed since
the read is rejected instead of misapplied.

## Why line numbers and not per-line hashes

A per-line short hash cannot address a file safely: two identical lines (`}`,
`return;`, a blank line) hash alike, so resolving "the tag that appears first"
edits whichever copy the scan reached. This was measured, not theorised — the
first implementation returned `"B\nb\nc\n"` for an edit meant for line 2.
Line numbers are unique by construction.

Freshness is a different question from identity, so it gets its own mechanism:
one tag per file.

## Notes and limits

- Naming does not touch the built-ins, so `read`/`edit`/`write` behave exactly as
  before. Nothing else in this repository can collide with `hash_read` or
  `hash_edit`.
- The tag ignores trailing spaces, tabs and CR per line, so a formatter run or a
  CRLF checkout does not invalidate a tag that was just read. Any other change
  does.
- Writes go through a temporary file in the same directory and a rename, so an
  interrupted write cannot truncate the file. Permissions are preserved. Hard
  links are not (the target is replaced, not written through).
- No syntax check, no dry run, no `replaceAll`, no multi-file apply.
