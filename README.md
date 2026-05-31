# cpp-navigator

LLM-optimized C++ codebase navigator. Answers three questions about a C++ source tree and emits strict JSON Lines for consumption by an LLM agent or script:

1. **`find-def`** — Where is X defined, and what is its exact text?
2. **`find-decl`** — Where is X declared, and what is its signature/doc?
3. **`find-refs`** — Where is X used, and (optionally) in what calling context?

**Zero required configuration. Zero network egress. Sub-second on large trees.**

## Why this tool exists

Feeding C++ to an LLM usually means one of two things:

| Approach | Token cost | Data boundary |
|---|---|---|
| Paste raw files | High — whole headers, whole TUs | Human controls everything |
| **cpp-navigator** | Low — only the precise slice | Human-controlled; no live reach |
| MCP / live tools | Low | Model has autonomous repository access |

The tool occupies the middle row: MCP-grade token efficiency for C++ without granting a model live, autonomous reach into your repository. Sprawling headers, template-heavy translation units, and library-scale trees make whole-file pasting expensive; per-symbol extraction removes that cost.

### Zero network egress

The tool performs only local file reads and local parsing. It never opens a network socket — no telemetry, no update checks, no remote indexing. This is a hard, tested invariant, not a configuration toggle. For organizations that decline MCP because they cannot verify data stays on the machine, this is the core guarantee: no code path can transmit source off-host.

### LLM workflow

```sh
# 1. Run a batch query
cpp-navigator find-def SetText ParseNode Widget \
  --root ./src --format bundle > context.md

# 2. Paste context.md into the chat
```

The `--format bundle` wraps all records in a single fenced block with a token estimate at the bottom. `--manifest` and `--budget` let you control exactly what crosses into the model's context window.

## Installation

```sh
cargo install --path .
```

A short alias `cppnav` is installed alongside `cpp-navigator`.

### Semantic backend (optional)

The default build uses tree-sitter and is fully self-contained (no system dependencies). For higher precision — accurate overload disambiguation, template resolution — enable the libclang backend:

```sh
cargo build --release --features semantic
```

Requires a system `libclang` (e.g. `brew install llvm`) and a `compile_commands.json` in the search root. The default tree-sitter backend is used automatically when `compile_commands.json` is absent.

## How it works

```
query
  │
  ▼
Stage 0: Candidate finder (ripgrep-class parallel walk)
  │   Fast text prefilter; narrows the tree to files mentioning the identifier.
  │   Respects .gitignore. Stops at --max-candidates distinct files.
  ▼
Stage 1: Syntactic engine (tree-sitter-cpp)
  │   Parses each candidate file; extracts byte-exact AST boundaries.
  │   Handles namespaces, templates, overloads, qualified names.
  ▼
Stage 2: Semantic engine (libclang) — opt-in via --semantic
      True type/overload resolution using compile_commands.json.
      Falls back to Stage 1 automatically when the DB is absent.
```

## Usage

```
cpp-navigator <COMMAND> <NAME>... [OPTIONS]
cppnav        <COMMAND> <NAME>... [OPTIONS]
```

### Commands

| Command | Description |
|---------|-------------|
| `find-def <name>` | Find the definition(s) of a symbol |
| `find-decl <name>` | Find the declaration/signature (header-biased; falls back to definitions for inline/local functions) |
| `find-refs <name>` | Find all references/usages |

All three commands accept multiple names and a `--manifest` file.

### Common options

| Flag | Default | Description |
|------|---------|-------------|
| `--root <PATH>` | `.` | Search root (repeatable) |
| `--format <FMT>` | `jsonl` | Output format: `jsonl`, `bundle`, `human` |
| `--max-results <N>` | `3` | Show full content for up to N overloads before switching to locations-only |
| `--max-candidates <N>` | `200` | Cap on candidate files before parsing |
| `--window <N>` | `10` | ±lines for fallback text windows |
| `--lang <EXT,...>` | all C/C++ | Restrict to these file extensions |
| `--no-ignore` | off | Ignore `.gitignore`/`.ignore` rules |
| `--manifest <PATH>` | — | Read additional query names from a file (one per line, `#` comments) |
| `--budget <N>` | — | Cap output at ~N tokens (selection-only trim, never edits payload bytes) |
| `--semantic` | off | Enable libclang Stage 2 (`--features semantic` required) |
| `--compile-db <PATH>` | auto | Path to `compile_commands.json` |
| `--jobs <N>` | #cores | Parser/walker threads |
| `--quiet` | off | Suppress stderr diagnostics |

#### `find-def` only

| Flag | Description |
|------|-------------|
| `--scope` | If the match is a class member, expand to the enclosing class/struct |

#### `find-refs` only

| Flag | Description |
|------|-------------|
| `--context` | Emit the enclosing function/template body of each hit (deduplicated by scope) |

## Examples

```sh
# Find where Widget::Draw is defined
cpp-navigator find-def Widget::Draw --root ./src

# Find the declaration with signature and doc comment, human-readable
cpp-navigator find-decl Draw --root ./src --format human

# Show all overloads of SetText (up to 5)
cpp-navigator find-def SetText --root ./src --max-results 5

# Find all usages of a function with enclosing scope bodies
cpp-navigator find-refs SetText --context --root ./src

# Batch query: multiple symbols in one pass
cpp-navigator find-def Widget Draw Resize --root ./src

# Query from a manifest file, output a bundle for pasting
cpp-navigator find-def --manifest queries.txt --root ./src --format bundle

# Restrict to header files only
cpp-navigator find-decl ParseNode --root ./src --lang h,hpp

# High-precision mode with compile_commands.json
cpp-navigator find-def MyTemplate --root ./build --semantic
```

## Output format

Every record is one JSON object on its own line. The envelope fields are always present:

```jsonc
{
  "schema_version": "1.0",
  "tool": "cpp-navigator",
  "command": "find-def",
  "target": "Widget::Draw",
  "status": "resolved",          // resolved | ambiguous | fallback | not_found
  "resolution_type": "function_definition",
  "engine": "tree-sitter"
}
```

Branch on `status` and `resolution_type` — never string-scrape content.

### Status values

| Status | When | Key fields |
|--------|------|------------|
| `resolved` | Engine bounded the target to one (or a few) exact constructs | `file_path`, `start_line`, `end_line`, `content`; `results[]` for overloads |
| `ambiguous` | Matches exceed `--max-results` | `candidates[]` with file/line/snippet |
| `fallback` | Text match but no parseable boundary | `file_path`, `approximate_line`, `content_buffer` |
| `not_found` | No textual match | `message` |

### Resolved — single match

```jsonc
{
  "status": "resolved",
  "resolution_type": "function_definition",
  "file_path": "src/widget.cpp",
  "start_line": 10,
  "end_line": 15,
  "start_byte": 120,
  "end_byte": 198,
  "content": "void Widget::Draw() {\n    // Draw implementation\n}"
}
```

### `find-decl` extras

```jsonc
{
  "status": "resolved",
  "resolution_type": "declaration",
  "signature": "void Draw()",
  "type": "void()",
  "doc": "/// Draw the widget on screen."
}
```

### Multiple overloads (`results` array)

When 2–N overloads are found and N ≤ `--max-results`, a single record carries a `results` array:

```jsonc
{
  "status": "resolved",
  "resolution_type": "function_definition",
  "results": [
    {
      "file_path": "src/widget.cpp",
      "start_line": 22, "end_line": 24,
      "content": "void Widget::SetText(const char* text) { ... }",
      "qualified_name": "ui::Widget::SetText"
    },
    {
      "file_path": "src/widget.cpp",
      "start_line": 26, "end_line": 30,
      "content": "void Widget::SetText(const char* text, int maxlen) { ... }",
      "qualified_name": "ui::Widget::SetText"
    }
  ],
  "message": "Found 2 matches."
}
```

### `find-refs` location-only (default)

```jsonc
{
  "status": "resolved",
  "resolution_type": "references",
  "locations": [
    { "file": "src/widget.cpp", "line": 10 },
    { "file": "src/main.cpp",   "line": 47 }
  ],
  "message": "Found 2 references."
}
```

### `find-refs --context`

```jsonc
{
  "status": "resolved",
  "resolution_type": "references_with_context",
  "contexts": [
    {
      "file": "src/main.cpp",
      "line": 47,
      "scope_start_line": 44,
      "scope_end_line": 55,
      "content": "void RenderFrame() {\n    w.SetText(\"hello\");\n    ...\n}"
    }
  ]
}
```

### Ambiguous (too many matches)

```jsonc
{
  "status": "ambiguous",
  "resolution_type": "ambiguous_multiple_matches",
  "message": "Found 4 candidates (exceeds --max-results 3). Returning locations only.",
  "candidates": [
    { "file_path": "src/parser.cpp", "line": 45, "snippet": "bool ParseNode(ASTContext* ctx) {" }
  ]
}
```

### Fallback (no parse boundary)

```jsonc
{
  "status": "fallback",
  "resolution_type": "partial_resolution_fallback",
  "file_path": "include/macros.h",
  "approximate_line": 88,
  "window_before": 10,
  "window_after": 10,
  "content_buffer": "// raw lines around line 88",
  "message": "Semantic extraction unavailable for this target; returning raw text window."
}
```

## Output formats

| `--format` | Use case |
|------------|----------|
| `jsonl` | Default. One JSON record per line; pipe or redirect to a file. |
| `bundle` | All records in a single ` ```json ` fence with a `~N tokens` footer. Paste this block directly into a chat. |
| `human` | ANSI-colored text for reading in a terminal. Enabled automatically when stdout is a TTY. |

## Degradation ladder

The tool never hard-fails when a best-effort answer is possible:

1. **Resolved** — engine found exact construct(s)
2. **Multi-resolved** — 2–N overloads shown in full via `results[]`
3. **Ambiguous** — too many matches; locations only via `candidates[]`
4. **Fallback** — text match but no AST boundary; raw ±window lines
5. **Not found** — no textual match in any searched file

`find-decl` additionally falls back from declarations to definitions when no forward prototype exists (e.g. inline methods, static functions in `.cpp`/`.inl` files without a separate header entry).

## Searched file extensions

Default: `c cc cpp cxx h hpp hh hxx inl`

Override with `--lang h,hpp` (comma-separated, no leading dot).

`find-decl` searches header files first (`h hpp hh hxx`), then falls back to all extensions if no header result is found — so local functions in `.cpp` and `.inl` files are covered.

## Qualified names

Targets may be bare (`Draw`) or qualified (`Widget::Draw`, `ui::Widget::Draw`). The prefilter always matches the bare final component for maximum recall; the engine then enforces the qualifier. Bare names return all overloads.

## Token budget

`--budget N` trims the output to approximately N estimated tokens using selection-only trimming: inner arrays are shortened before whole records are dropped. Payload bytes are never edited — fidelity is an invariant.

## Building and testing

```sh
cargo build --release
cargo test
```

Integration tests run against a small fixture repo under `tests/fixtures/sample/`. The zero-egress test (`zero_egress_no_network`) verifies the binary produces no network traffic under `sandbox-exec` on macOS.
