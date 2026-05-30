# Product Requirements Document (PRD)

**Project:** LLM-Optimized C++ Codebase Navigator CLI
**Target User:** Large Language Model (LLM) Agents

## 1. Overview

The objective is to build a high-performance command-line interface (CLI) tool designed exclusively for LLM consumption. The tool will rapidly query massive C++ repositories to extract exact semantic boundaries, such as function definitions, variable declarations, and usage references. All output is strictly optimized for programmatic parsing and token economy, ensuring seamless integration with advanced reasoning models like Qwen 3.6.

---

## 2. Core Features & Capabilities

### 2.1. Definition Extraction

* **Target:** Functions, variables, and templates.
* **Behavior:** Locates the exact file, start line, and end line of the target identifier.
* **Output:** Returns the file path, bounding line numbers, and the raw text of the complete definition.

### 2.2. Declaration & Documentation Retrieval

* **Target:** Header files (`.h`, `.hpp`).
* **Behavior:** Locates the forward declaration or signature of a function or variable.
* **Output:** Returns the declaration signature, variable type, and any adjacent inline comments or docstrings (e.g., Doxygen blocks).

### 2.3. Contextual Scope Expansion

* **Target:** Class methods and member variables.
* **Behavior:** Offers a flag to contextually pull the encompassing definition if the queried target belongs to a class or struct.
* **Output:** Returns the entire class, struct, or template definition housing the requested member.

### 2.4. Reference & Usage Search

* **Target:** Function calls and variable accesses across the codebase.
* **Location-Only Mode:** Returns a dense, token-efficient list of file paths and line numbers where the target is referenced.
* **Contextual Mode:** Returns the surrounding scope, pulling the entire function or template body in which the target is being utilized to provide caller logic.

---

## 3. Non-Functional Requirements (NFRs)

### 3.1. Performance & Latency

* **Execution Time:** Queries must execute in milliseconds. Performance testing should validate sub-second response times even on large-scale source directories underlying massive shared libraries like `libcore.so`.
* **Delegation Strategy:** The tool must delegate raw text and regex searching to hyper-optimized utilities like `ripgrep` to narrow down candidate files before initiating heavier semantic parsing.

### 3.2. Data Fidelity & Token Economy

* **Absolute Source Fidelity:** The tool must never modify, trim, or normalize the raw text extracted from the codebase.
* **Whitespace Preservation:** Indentation and newline characters must match the original source file byte-for-byte to ensure diff-generation and patch tools work flawlessly.
* **Structural Efficiency:** Token economy is achieved strictly through concise metadata and the omission of irrelevant surrounding code, never by massaging the target payload.

### 3.3. Parsing Accuracy

* **C++ Complexities:** The tool must accurately resolve template metaprogramming, method overloading, and namespaces using Abstract Syntax Tree (AST) parsing.

---

## 4. Interface & Output Specification

### 4.1. Proposed Command Interface

* `find-def <name> [--scope]`
* `find-decl <name>`
* `find-refs <name> [--context]`

### 4.2. Output Format (JSON Lines)

All standard output must be formatted as strict JSON Lines (JSONL) to allow the orchestrating agent to stream and parse results programmatically without string-matching heuristics.

**Standard Output Schema Example (`find-def`):**

```json
{
  "target": "InitializeMemoryPool",
  "resolution_type": "function_definition",
  "file_path": "src/core/memory.cpp",
  "start_line": 142,
  "end_line": 165,
  "content": "void InitializeMemoryPool(size_t pool_size) {\n    std::lock_guard<std::mutex> lock(pool_mutex);\n    // raw code with preserved whitespace\n}"
}

```

---

## 5. Error Handling & Graceful Degradation

### 5.1. The "Graceful Degradation" Principle

If a query cannot be resolved to a single definitive semantic boundary, the tool must prioritize "best-effort" responses over hard failures to keep the LLM's context alive.

### 5.2. Output Specification: Ambiguous Results

When encountering overloaded identifiers, the tool changes the `resolution_type` and provides an array of candidates.

**Ambiguous Schema Example:**

```json
{
  "target": "ParseNode",
  "resolution_type": "ambiguous_multiple_matches",
  "message": "Target is ambiguous. Found 3 overloads/definitions. Returning raw candidate locations.",
  "candidates": [
    {
      "file_path": "src/parser/ast_unix.cpp",
      "line": 45,
      "snippet": "bool ParseNode(ASTContext* ctx, const Token& t) {"
    }
  ]
}

```

### 5.3. Output Specification: Fallback Search

If the target is found via text search but the AST parser cannot confidently extract exact boundaries, the tool returns the surrounding raw text buffer.

**Fallback Schema Example:**

```json
{
  "target": "MAGIC_MACRO_INIT",
  "resolution_type": "partial_resolution_fallback",
  "message": "Semantic extraction failed. Returning raw text search buffer.",
  "file_path": "include/core/macros.h",
  "approximate_line": 88,
  "content_buffer": "// 10 lines above and below line 88 providing unparsed context"
}

```

---

## 6. Architectural Options (Design Phase Evaluation)

### 6.1. Option A: The Compiler-Driven Approach

* **Mechanism:** Parses a `compile_commands.json` file using an actual compiler frontend (`clangd` / libclang).
* **Pros:** Guarantees absolute accuracy for deep template metaprogramming, complex macros, and cross-file namespace resolution.
* **Cons:** Requires a configured and built repository, resulting in higher friction, slower initial indexing, and a heavier memory footprint.

### 6.2. Option B: The Syntax-Driven Approach

* **Mechanism:** Combines `ripgrep` for instant file identification with `tree-sitter` for rapid AST generation and boundary extraction.
* **Pros:** Zero configuration required, allowing immediate querying of uncompiled repositories with blisteringly fast execution.
* **Cons:** Lacks true semantic understanding, which may lead to struggles with heavily macro-obfuscated definitions or highly ambiguous template instantiations.
