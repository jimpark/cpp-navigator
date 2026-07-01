# Agent skills

`cpp-navigator/SKILL.md` teaches an LLM coding agent (Claude Code, GitHub
Copilot, or anything else that speaks the [agentskills.io](https://agentskills.io)
format) how to use this tool to explore a **C/C++ codebase** efficiently once
`cpp-navigator`/`cppnav` is installed there.

It lives here — tracked in this repo, versioned alongside the CLI it
documents — rather than in `.github/skills/`, `.claude/skills/`, or
`.agents/skills/`, so it is not auto-loaded by agents working on
*cpp-navigator's own source* (this repo is a Rust project; the skill is
about navigating C/C++ targets and would just be noise here).

## Using it in a C/C++ project

Copy or symlink the `cpp-navigator` directory into the target project's
skills folder, e.g.:

```sh
# Claude Code
cp -r skills/cpp-navigator /path/to/your-cpp-project/.claude/skills/cpp-navigator

# GitHub Copilot
cp -r skills/cpp-navigator /path/to/your-cpp-project/.github/skills/cpp-navigator
```

The agent picks it up automatically the next time it works in that repo,
provided the `cpp-navigator`/`cppnav` binary is on `PATH` there.
