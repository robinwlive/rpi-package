# Changelog

## 0.2.0

Rename the tools to `hash_read` / `hash_edit` and stop shadowing the built-ins.

Measured reasons (see README): reading costs +16% bytes, a single-line edit costs
more than `oldText` because of the hunk header, and `replaceAll` has no
line-number equivalent. The built-in `read`/`edit`/`write` stay available, so the
model picks per task instead of one pair forcing the other out.

## 0.1.0

First release: `read`/`edit` replacing the built-ins, line-number addressing,
whole-file snapshot tag, same-directory atomic rename.
