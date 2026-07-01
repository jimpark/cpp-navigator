---
name: cpp-navigator
description: Look up C/C++ symbol definitions, declarations, and references with cpp-navigator (cppnav) instead of grepping or pasting whole files. Use whenever exploring, reading, or answering questions about a C/C++ codebase where the cpp-navigator or cppnav binary is available on PATH — especially on large trees where reading whole headers/TUs would burn excessive context.
allowed-tools: Bash(cpp-navigator:*), Bash(cppnav:*)
metadata:
  homepage: https://github.com/jimpark/cpp-navigator
---

# cpp-navigator

`cpp-navigator` (alias `cppnav`) answers three questions about a C/C++ source
tree and returns strict JSON Lines: where is a symbol **defined**, where is it
**declared**, and where is it **used**. It parses only the files that match,
and returns byte-exact source slices instead of forcing you to open and read
whole files. Prefer it over `grep`/`cat`/reading whole headers whenever one is
available — it is dramatically cheaper in tokens and immediately structured.

## When to use this skill

- You need to see the body of a function/class/struct/method by name.
- You need a declaration's signature and doc comment (e.g. from a header).
- You need every call site / usage of a symbol, optionally with the
  enclosing function body for context.
- You're orienting in an unfamiliar C/C++ repo and want targeted slices
  instead of paging through files.

Before relying on it, confirm the binary exists: `cpp-navigator --version`
(or `cppnav --version`). If it's missing, fall back to normal file
reading/search tools — do not try to install it as part of an unrelated task.

## Core commands

```
cpp-navigator find-def  <NAME>... [OPTIONS]   # definition(s)
cpp-navigator find-decl <NAME>... [OPTIONS]   # declaration/signature (header-biased)
cpp-navigator find-refs <NAME>... [OPTIONS]   # usages/references
```

All three accept multiple symbol names in one invocation — batch related
lookups into a single call rather than issuing one process per symbol.
Names may be bare (`Draw`) or qualified (`Widget::Draw`, `ui::Widget::Draw`);
bare names return all overloads.

## Options that matter for an agent

| Flag | Why you'd use it |
|------|-------------------|
| `--root <PATH>` | Point at the subtree to search (repeatable). Narrow this to cut noise on large repos. |
| `--format jsonl` | Default; one JSON object per line — parse each line yourself. |
| `--max-results <N>` | Raise this if you expect several overloads and want them all inlined instead of collapsed to `ambiguous`. |
| `--include content,offsets,type` | Opt into heavier fields when the default structured summary (signature/doc) isn't enough. |
| `--context` (`find-refs` only) | Include the enclosing function/template body at each call site, not just the line. |
| `--scope` (`find-def` only) | Expand a class-member match to the whole enclosing class/struct. |
| `--manifest <PATH>` | Read a batch of symbol names from a file instead of the command line. |
| `--budget <N>` | Cap output to ~N estimated tokens via selection-only trimming (never edits payload bytes) — use when a query might return a lot. |
| `--semantic` | Opt into libclang-backed resolution for precise overload/template disambiguation, if a `compile_commands.json` and a semantic-enabled build are available. Silently falls back to the tree-sitter engine otherwise (check the `engine` field in the output to confirm which ran). |
| `--lang h,hpp` | Restrict to specific extensions, e.g. headers-only when hunting a declaration. |

Full flag reference: run `cpp-navigator --help` or `cpp-navigator <command> --help`.

## Reading the output

Every record has an envelope with `status` and `resolution_type`. **Branch on
`status` — never string-scrape `content`.**

| `status` | Meaning | Where to look |
|----------|---------|----------------|
| `resolved` | Exact construct(s) found | `content`/`signature`/`doc`, or a `results[]` array if there are several overloads |
| `ambiguous` | More matches than `--max-results` | `candidates[]` (file/line/snippet only) — re-run with a qualified name or a higher `--max-results` if you need full bodies |
| `fallback` | Text match, no parseable AST boundary | `content_buffer` — a raw ± `--window` line slice around `approximate_line` |
| `not_found` | No textual match anywhere searched | nothing else to do; the symbol isn't in the searched roots/extensions |

`find-refs` without `--context` returns `locations: [{file, line}, ...]` only
— add `--context` when you need to reason about how each call site is used,
not just where it is.

## Recommended workflow

1. Start narrow: `find-def`/`find-decl` on the specific symbol you need, with
   `--root` scoped to the relevant subtree.
2. Batch related symbols in one call instead of N separate invocations:
   `cpp-navigator find-def Widget Draw Resize --root ./src`.
3. If you're about to paste results into a chat/report for a human rather
   than parsing them yourself, use `--format bundle` — it fences everything
   in one block with a token-count footer.
4. If a query might be broad, set `--budget` up front rather than discovering
   the output was too large after the fact.
5. On `ambiguous`, tighten the query (qualify the name, e.g. `ui::Widget::Draw`)
   rather than blindly raising `--max-results`.

## Example

```sh
# What does Widget::Draw actually do?
cpp-navigator find-def Widget::Draw --root ./src

# Where is it declared, with its doc comment?
cpp-navigator find-decl Draw --root ./src

# Every call site, with enclosing function context
cpp-navigator find-refs Draw --context --root ./src
```
