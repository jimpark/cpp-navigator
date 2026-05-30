# Design Specification — `cpp-navigator`

**Component:** LLM-Optimized C++ Codebase Navigator CLI
**Invocation:** canonical `cpp-navigator`; short human alias `cppnav`
**Status:** Draft v0.2 (for review)
**Derived from:** Product Requirements Document — *LLM-Optimized C++ Codebase Navigator CLI*
**Audience:** Implementers, reviewers

> This document translates the PRD's *what* into a concrete *how*. Sections flagged
> **[DECISION]** mark forks where a reasoned default was chosen but confirmation is
> wanted; they are collected in §15.

---

## 1. Purpose & Scope

`cpp-navigator` is a single, self-contained CLI binary that answers three classes of
question about a C++ source tree and emits the answers as strict JSON Lines for
consumption by an LLM agent:

1. *Where is X defined, and what is its exact text?* (`find-def`)
2. *Where is X declared, and what is its signature/doc?* (`find-decl`)
3. *Where is X used, and (optionally) in what calling context?* (`find-refs`)

In scope: identifier resolution for functions, variables, templates, classes/structs,
and their members; byte-exact text extraction; ambiguity reporting; graceful
degradation to raw-text fallback; local, token-efficient context delivery.

Out of scope (v1): refactoring/rewriting, call-graph construction, type-hierarchy
queries, cross-language sources, build-system orchestration.

## 2. Goals & Non-Goals

**Goals**
- Sub-second latency for point queries on large trees, via aggressive prefiltering.
- Byte-for-byte fidelity of extracted payloads (no trimming/normalization).
- Stable, versioned, machine-parseable output that never requires string-scraping.
- Zero required configuration for the common case.
- **Zero network egress.** Only local file reads and local parsing/subprocess work;
  the tool never opens a network socket. A hard, tested invariant (§3.1, §14).
- Never hard-fail when a best-effort answer is possible.

**Non-Goals**
- Being a general-purpose `grep`. Text search is an internal stage, not the product.
- Full semantic correctness in the absence of build information (offered as an
  opt-in precision mode instead — see §4).
- Granting a model live or autonomous access to the repository (see §3).

## 3. Deployment & Trust Model

`cpp-navigator` is positioned deliberately between the two common ways an LLM
consumes a C++ codebase:

| Mode | Data boundary | Token cost | Human effort |
|---|---|---|---|
| Raw LLM (paste files) | Human controls everything | High — whole files/headers | High |
| **cpp-navigator** | Local, human-controlled boundary; nothing autonomous | Low — only the precise slice | Low–medium |
| MCP / live tools | Model has autonomous access; relies on trusting the integration | Low | Lowest |

The tool's reason to exist is the middle row: MCP-grade token efficiency for C++
without granting a model live, autonomous reach into the repository. The human (or a
non-autonomous local wrapper script) runs the tool and decides exactly what crosses
into the model's context. C++ is the high-value case here — sprawling headers,
template-heavy translation units, and library-scale trees make whole-file pasting
ruinously expensive, and per-symbol extraction is the lever that removes it.

### 3.1 Zero network egress (guaranteed)
The tool performs only local file reads and local parsing/subprocess work. It never
opens a network socket — no telemetry, no update checks, no remote indexing. This is
a hard, tested invariant (§14), not a configuration toggle. For organizations that
decline MCP because they cannot be sure data stays on the machine, this is the core
guarantee: there is no code path that can transmit source off-host.

> Honest framing: MCP *can* run fully locally (stdio transport), so "no MCP" is not by
> itself a data-safety property. The defensible claim is the one above — provable zero
> egress plus a human-controlled boundary — which is what survives a security review.

### 3.2 Transport: human-in-the-loop
Because no live channel exists, output is designed to be carried by a human (or a thin
local wrapper) into the model:
1. Run `cpp-navigator find-def Foo --format bundle`.
2. Copy the single emitted block into the chat.

The bundle format (§8.8) and batch/manifest queries (§8.9) exist to make this one
copy-paste instead of many, and the token budget (§8.10) keeps a paste within a known
context cost.

### 3.3 Optional local MCP front-end (opt-in, future scope)
For organizations that *do* permit MCP, a thin stdio-MCP front-end can expose the same
engine over a local-only transport — same zero-egress core, different door. This is
explicitly opt-in and not built in v1; the standalone CLI remains the default and only
required mode. See §16.

## 4. Architecture Overview

The PRD presents Option A (compiler-driven, libclang) and Option B (syntax-driven,
ripgrep + tree-sitter) as alternatives. The tension is real: the NFRs demand
zero-config + millisecond latency (favoring B), while the PRD's Parsing Accuracy
requirement (§3.3 of the PRD) demands accurate resolution of overloads, templates, and
namespaces (favoring A, which alone has true type/overload semantics).

**[DECISION] Recommended resolution: a layered hybrid with B as the default engine and
A as an opt-in "precision backend."** This maps cleanly onto the graceful-degradation
ladder the PRD already describes (PRD §5), so it costs little conceptual overhead. All
stages are strictly local, consistent with §3.1.

```
              ┌─────────────────────────────────────────────┐
  query ───▶  │  Stage 0: Candidate Finder (ripgrep-class)   │  fast text prefilter
              └───────────────────────┬─────────────────────┘  → candidate {file,line}
                                      │
              ┌───────────────────────▼─────────────────────┐
              │  Stage 1: Syntactic Engine (tree-sitter)     │  default; per-candidate
              │  precise *syntactic* boundaries + name match │  AST boundary extraction
              └───────────────────────┬─────────────────────┘
                                      │  if --semantic AND compile_commands.json present
              ┌───────────────────────▼─────────────────────┐
              │  Stage 2: Semantic Engine (libclang/clangd)  │  opt-in precision
              │  true overload/template/type resolution      │  disambiguation
              └───────────────────────┬─────────────────────┘
                                      │
              ┌───────────────────────▼─────────────────────┐
              │  Stage 3: Fallback (raw text buffer)         │  if 1/2 can't bound it
              └───────────────────────┬─────────────────────┘
                                      ▼
                          Output Serializer (§8)
```

Rationale:
- ripgrep prefiltering is the reason no persistent index is needed for point queries
  (§10): only files that *contain the identifier* are ever parsed.
- tree-sitter gives precise, byte-accurate node boundaries with zero build setup — it
  answers "what are the exact start/end of this construct" perfectly, which is the
  dominant requirement. Its limit is *semantic* disambiguation.
- libclang is invoked only when the user opts in and a `compile_commands.json` exists,
  upgrading ambiguous syntactic results to fully-resolved ones.
- Each downward arrow is a degradation step, so the PRD's §5 behavior falls out
  naturally.

If you prefer to ship B-only for v1 and defer the libclang backend, the engine
abstraction (§5) keeps that a drop-in addition. See §15.

## 5. Component Breakdown

| Component | Responsibility |
|---|---|
| **CLI front-end** | Parse args/flags, validate, select command, configure output. |
| **Query planner** | Translate a command into a pipeline plan (which stages, what to capture). |
| **Candidate finder** | Locate files+lines that mention the target identifier (text-level). |
| **Engine trait** | Common interface implemented by `SyntacticEngine` and `SemanticEngine`. |
| **Boundary extractor** | Given a candidate node, compute exact `[start_byte,end_byte]` span. |
| **Resolver** | Decide resolved / ambiguous / fallback / not-found from candidate set. |
| **Doc/signature extractor** | For `find-decl`: pull signature, type, adjacent comments/Doxygen. |
| **Context extractor** | For `find-refs --context`: find enclosing function/template of each hit. |
| **Serializer** | Emit results (JSONL or bundle) with byte-faithful payloads. |

The **engine trait** is the key extensibility seam:

```
trait Engine {
    fn definitions(target, candidates) -> Vec<Resolution>;
    fn declarations(target, candidates) -> Vec<Resolution>;
    fn enclosing_scope(file, byte_offset) -> Option<Span>;   // for --scope / --context
    fn name() -> &str;                                        // "tree-sitter" | "libclang"
}
```

## 6. Internal Data Model

```
Span        { start_byte, end_byte, start_line, end_line, start_col, end_col }
SourceRef   { file_path, span }
Symbol      { name, qualified_name?, kind, signature?, type?, doc? }
Resolution  { symbol, source_ref, content_bytes, engine, confidence, status }
```

- `kind` ∈ { function, variable, template, class, struct, method, member, macro }.
- `content_bytes` is the *verbatim* byte slice `file[start_byte..end_byte]` — see §8.4.
- `status` ∈ { resolved, ambiguous, fallback, not_found } drives `resolution_type`.

## 7. Per-Command Pipelines

### 7.1 `find-def <name> [--scope]`
1. Candidate finder → files+lines mentioning `name`.
2. Syntactic engine parses each candidate file; collects nodes whose declarator name
   equals `name` and whose kind is *definition* (has a body / initializer).
3. If `--scope` and the match is a class member, expand the result span to the
   enclosing `class_specifier` / `struct_specifier` / template.
4. Resolver: 1 match → `resolved`; >1 → `ambiguous`; 0 definitions but text hits →
   `fallback`; 0 hits → `not_found`.

### 7.2 `find-decl <name>`
1. Candidate finder, but bias toward header files (`.h`, `.hpp`, `.hh`, `.hxx`) first;
   fall back to all files if none.
2. Syntactic engine collects *declaration* nodes (signature, no body) for `name`.
3. Doc/signature extractor attaches `type`, `signature`, and the adjacent leading
   comment block (line `//` runs or `/** ... */` Doxygen blocks immediately above).
4. Resolver as in 7.1.

### 7.3 `find-refs <name> [--context]`
1. Candidate finder produces *all* hits (this is inherently the answer set).
2. **Location-only mode (default):** emit a dense list of `{file_path, line}` — minimal
   tokens, no parsing of bodies required.
3. **`--context` mode:** for each hit, the engine finds the enclosing function/template
   body and emits that span + content.

## 8. Output Specification

### 8.1 Principles
- Default output is one JSON object per line; no pretty-printing; UTF-8.
- A stable **common envelope** on every record so the agent can branch on `status` /
  `resolution_type` without heuristics.
- Omit empty/irrelevant fields entirely (token economy is via *metadata concision and
  omission*, never by altering the payload — per PRD §3.2).

### 8.2 Common envelope (every record)
```json
{
  "schema_version": "1.0",
  "tool": "cpp-navigator",
  "command": "find-def",
  "target": "InitializeMemoryPool",
  "status": "resolved",
  "resolution_type": "function_definition",
  "engine": "tree-sitter"
}
```

### 8.3 Resolved definition / declaration
```json
{
  "schema_version": "1.0",
  "command": "find-def",
  "target": "InitializeMemoryPool",
  "status": "resolved",
  "resolution_type": "function_definition",
  "engine": "tree-sitter",
  "file_path": "src/core/memory.cpp",
  "start_line": 142,
  "end_line": 165,
  "start_byte": 4821,
  "end_byte": 5403,
  "content": "void InitializeMemoryPool(size_t pool_size) {\n    std::lock_guard<std::mutex> lock(pool_mutex);\n}"
}
```
`find-decl` adds `signature`, `type`, and `doc` and typically omits a body:
```json
{
  "resolution_type": "declaration",
  "signature": "void InitializeMemoryPool(size_t pool_size);",
  "type": "void(size_t)",
  "doc": "/// Allocate the global pool. Must be called once at startup."
}
```

### 8.4 Byte-fidelity contract
- `content` is the exact byte range `[start_byte, end_byte)` of the file, JSON-string
  escaped *only* as JSON requires (`\n`, `\t`, `\"`, `\\`, control chars). No CRLF→LF
  normalization, no dedent, no trim.
- `start_byte`/`end_byte` are always emitted so an agent can re-slice from disk and
  verify a perfect round-trip independent of the escaped string.

### 8.5 Ambiguous (overloads / multiple definitions)
```json
{
  "status": "ambiguous",
  "resolution_type": "ambiguous_multiple_matches",
  "target": "ParseNode",
  "message": "Found 3 candidates. Returning raw candidate locations.",
  "candidates": [
    { "file_path": "src/parser/ast_unix.cpp", "line": 45,
      "snippet": "bool ParseNode(ASTContext* ctx, const Token& t) {" }
  ]
}
```
> In `--semantic` mode, Stage 2 may collapse this to a single `resolved` record when
> the build graph disambiguates the call site; otherwise candidates are returned for
> the agent to choose.

### 8.6 Fallback (text found, boundary not resolvable)
```json
{
  "status": "fallback",
  "resolution_type": "partial_resolution_fallback",
  "target": "MAGIC_MACRO_INIT",
  "message": "Semantic extraction failed; returning raw text window.",
  "file_path": "include/core/macros.h",
  "approximate_line": 88,
  "window_before": 10,
  "window_after": 10,
  "content_buffer": "...verbatim ±N lines around line 88..."
}
```

### 8.7 Not found
```json
{ "status": "not_found", "resolution_type": "not_found", "target": "Foo",
  "message": "No textual or semantic match in the searched roots." }
```

### 8.8 Output profiles (`--format`)
Identical record *data* in both profiles; the profile is a presentation wrapper, never
a payload change (§8.4 still holds).
- **`jsonl` (default):** one record per line. For programmatic consumption or a local
  wrapper that injects results.
- **`bundle`:** a single fenced block suitable for a human to paste into a chat. With
  `--legend`, the block is prefixed with a short, one-time key explaining the record
  fields so the model interprets them without the human authoring instructions. Bundle
  output ends with a footer line carrying an estimated token count for the block.

### 8.9 Batch / manifest queries
To minimize human round trips, multiple queries can run in one invocation:
- Repeated targets on the command line, or `--manifest <path>` with one query per line
  (e.g. `find-def Foo`, `find-refs Bar --context`).
- Results are concatenated and de-duplicated. In `bundle` mode they are grouped under a
  short per-query header. One run → one paste.

### 8.10 Token budget
- `--budget <n>` caps the estimated tokens of the emitted set. The tool trims by
  *selection only* — preferring location-only over contextual records, capping
  candidates, dropping lowest-confidence matches — and **never** by editing payload
  bytes (§8.4 is absolute). When trimming occurs, a `budget_trimmed: true` marker and a
  short message are emitted so the consumer knows the set is partial.

## 9. Error Handling, Degradation & Exit Codes

**[DECISION] Degradation ladder** (per PRD §5.1 — keep the agent's context alive):
`semantic resolve → syntactic resolve → ambiguous candidates → text fallback → not_found`.
A query never crashes the tool when any rung produces a record.

**[DECISION] Exit codes:**
| Code | Meaning |
|---|---|
| 0 | A well-formed answer was produced — *including* `not_found` (it's a valid answer). |
| 2 | Usage error (bad flags, missing target, unreadable root). |
| 3 | Internal tool error (parser crash, I/O failure mid-run). |

Reasoning: collapsing `not_found` into exit 0 keeps agent control flow simple — the
agent always reads stdout and branches on `status`. (Alternative: distinct code for
`not_found` — flagged in §15.) Diagnostics go to **stderr** only, never stdout, so
stdout stays pure output.

## 10. Performance Design

- **Prefilter, then parse.** Stage 0 narrows a huge tree to the handful of files
  containing the identifier; only those are parsed. This is what makes a persistent
  index unnecessary for point queries.
- **[DECISION] No persistent index in v1.** Rely on prefilter + an in-process parse
  cache (a file is parsed at most once per invocation even with many hits). A
  persistent symbol index is deferred to future work (§16).
- **Parallelism.** Candidate files parsed across a thread pool; results merged
  deterministically (sorted by `file_path`, then `start_byte`) for stable output.
- **Respect ignore rules.** Honor `.gitignore`/`.ignore` and skip binary files,
  matching ripgrep semantics, to avoid scanning build artifacts.
- **Worst case & mitigation.** A very common identifier yields thousands of candidate
  files. Mitigate with `--max-candidates N` (default e.g. 200) and emit a
  `truncated: true` flag plus a `message` rather than blocking. For `find-refs`,
  location-only mode avoids body parsing entirely.

## 11. C++-Specific Handling

- **Name matching.** Declarator-name extraction must peel pointers/references,
  qualified names (`A::B::f`), and operator names. Matching supports both bare (`f`)
  and qualified (`A::B::f`) targets.
- **Overloads.** Multiple same-name definitions → `ambiguous` with all candidate
  signatures (Stage 1), or disambiguated in Stage 2.
- **Templates.** Capture the full `template_declaration` span (the `template<...>`
  prefix through the body) as the definition boundary, not just the inner function.
- **Namespaces.** Track enclosing `namespace_definition` to compute `qualified_name`;
  allow filtering by namespace prefix.
- **Header/source pairing.** `find-decl` prefers headers; `find-def` prefers TUs but
  searches both. Inline definitions in headers are valid `find-def` results.
- **Macros.** tree-sitter does not expand the preprocessor; macro-defined or
  macro-obfuscated symbols are the primary `fallback` case (§8.6), or resolved in
  `--semantic` mode where the preprocessor has run.

## 12. CLI Specification

```
cpp-navigator <command> <name>... [options]    (alias: cppnav)

Commands:
  find-def   <name> [--scope]
  find-decl  <name>
  find-refs  <name> [--context]

Global options:
  --root <path>            Search root (default: cwd). Repeatable.
  --semantic               Enable Stage 2 (requires compile_commands.json).
  --compile-db <path>      Explicit path to compile_commands.json.
  --lang <ext,...>         Restrict to extensions (default: c,cc,cpp,cxx,h,hpp,hh,hxx).
  --max-candidates <n>     Cap candidate files (default 200).
  --window <n>             Fallback context window in lines (default 10).
  --jobs <n>               Parser threads (default: #cores).
  --no-ignore              Do not honor .gitignore/.ignore.
  --format <jsonl|bundle>  Output profile (default jsonl; §8.8).
  --legend                 In bundle mode, prepend a one-time field legend.
  --manifest <path>        Run multiple queries from a file, one per line (§8.9).
  --budget <n>             Cap estimated output tokens; selection-only trim (§8.10).
  --quiet                  Suppress stderr diagnostics.
```

Invocation: ships as `cpp-navigator` with a short alias `cppnav` for interactive human
use; the canonical, LLM-invoked form is the long name. Output is the chosen profile on
stdout; no human-pretty mode in v1 (a `--pretty` debug flag is optional, §15).

## 13. Implementation Language & Dependencies

**[DECISION] Recommended: Rust.** It yields a single static binary (zero-config
deploy, instant startup, easy cross-compilation), has first-class `tree-sitter`
bindings, and lets us link ripgrep's own engine crates (`ignore`, `grep-searcher`,
`grep-regex`) directly instead of shelling out to an external `rg` — preserving the
self-contained, zero-dependency, zero-egress properties. libclang integration is
available via `clang`/`clang-sys` for the optional Stage 2.

**Python as a prototype option.** Python can meet the *sub-second* bar for typical
point queries because the heavy stages are native regardless of glue language (text
search via ripgrep; parsing via tree-sitter's C core). Guardrails if chosen: keep the
Python layer thin glue; drive tree-sitter through its **query API** (`.scm` captures)
to keep hot loops in C; expect that per-invocation interpreter startup (tens to
~100ms+) makes the literal "milliseconds" target hard on a cold process and compounds
across an agent's many calls — the escape hatch is a resident **daemon mode** (pay
startup once), or a later localized port of the engine. Tradeoffs vs Rust: faster
iteration and a lower contributor bar, but harder single-artifact distribution (native
wheels are per-platform) and weaker CPU-bound parallelism under the GIL. See §15.

Key dependencies (Rust path): `tree-sitter` + `tree-sitter-cpp`, `ignore` + `grep-*`,
`serde_json`, `clap`, `rayon`, and optionally `clang` for Stage 2.

## 14. Testing Strategy

- **Golden tests:** fixture repos with known symbols → asserted output records.
- **Fidelity tests:** assert `file[start_byte..end_byte]` re-sliced from disk equals the
  JSON-unescaped `content` byte-for-byte, including CRLF and tabs.
- **Ambiguity/overload fixtures:** multiple definitions, templates, operator overloads,
  namespaces.
- **Degradation fixtures:** macro-defined symbols → `fallback`; unknown → `not_found`.
- **Schema validation:** every emitted record validated against a published JSON Schema
  for `schema_version`.
- **Zero-egress test (gates §3.1):** run representative queries under a network-disabled
  sandbox (e.g. `unshare -n`) and under a syscall monitor asserting no `socket`/`connect`
  to non-local addresses. CI fails if any network syscall appears. This makes zero
  egress a verified property, not a claim.
- **Performance benchmarks:** large synthetic tree; assert sub-second point queries.
- Optional: fuzzing the parser/serializer for crash-freedom (supports §9's no-hard-fail
  goal).

## 15. Open Decisions (please confirm)

1. **Architecture scope:** ship the hybrid (B default + opt-in A) as designed, or B-only
   for v1 with the engine seam (§5) left open for A later?
2. **Implementation language:** Rust (recommended, §13), or Python-as-prototype with the
   §13 guardrails?
3. **Persistent index:** confirm none in v1 (prefilter-only), or do you need a
   warm-index/daemon mode for very high query volume? (Note: daemon mode is also the
   Python latency escape hatch.)
4. **Exit-code semantics:** is `not_found` = exit 0 acceptable, or should it have a
   distinct non-zero code for agent control flow?
5. **Embed vs shell out** for the text-search stage (embedding ripgrep crates keeps a
   single dependency-free binary and reinforces zero-egress; shelling out to `rg` is
   simpler but adds an install-time dependency — slightly at odds with "zero-config").

## 16. Future Work (out of scope for v1)

- Optional local stdio-MCP front-end over the same engine (§3.3), for orgs that permit
  MCP.
- Persistent/incremental symbol index and an optional resident daemon.
- Call-graph and type-hierarchy queries.
- Additional output profiles (e.g., a `--diff-anchor` mode emitting stable anchors for
  patch generation).
