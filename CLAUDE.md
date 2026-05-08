# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

## 5. Rust-Specific Guidelines

**Use the type system. Keep functions small. Make business logic read like pseudocode.**

### Types over strings and `Value`

- Replace stringly-typed parameters (`&str` for chains, IDs, hex bytes) with enums and newtypes.
- Replace `serde_json::Value` in business logic with `#[derive(Deserialize)]` structs. `Value` is a parsing-boundary tool; once data is in the program, it has a shape — give it one.
- Make illegal states unrepresentable. If a field is required for one variant and absent for another, that's an enum, not an `Option`.

### Function shape

- One function does one thing. The name describes that thing exactly. If you need a comment to explain *what* it does, the name is wrong.
- Extract for readability *even when not reused*. A 30-line block with a clear name beats a 30-line inline block.
- Ceilings (enforced by clippy, not vibes): cognitive complexity ≤ 25, lines ≤ 100, arguments ≤ 7.
- More than 7 arguments → group them in a struct (e.g., `PollPipelineArgs`). The struct doc replaces the argument-list comments.

### Business logic reads like pseudocode

- The top-level orchestrator (`run`, `verify_onchain`, `relay_to_destination`) should be a sequence of named calls — a reader scanning it understands the feature without going deeper.
- Push details (RPC parsing, retry loops, byte slicing) into helpers named for what they produce, not how.
- When a function exceeds ~100 lines, that's a signal it's holding multiple responsibilities — split before adding more.

### Module layout

- Types live in their own modules. A file mixing `pub struct Foo`, `impl Foo`, and `pub async fn business_workflow(...)` is doing two jobs.
- Pattern: `feature/{types.rs, helpers.rs, mod.rs}` where `mod.rs` is the orchestrator and the others hold reusable building blocks. Keep file scope narrow.
- Imports always at the top of the module; never `use` inside a function body.

### Deterministic over LLM

- If a rule can be expressed as a lint or a hook, do that — don't rely on review or memory to catch it.
- Before adding a guideline here, ask: "Can clippy/rustfmt/a git hook enforce this?" If yes, configure the tool. CLAUDE.md is for what the tooling can't catch.
- Existing deterministic gates: `[lints.clippy]` in `Cargo.toml`, `.githooks/pre-commit` (fmt + clippy + tests on default features), `.githooks/pre-push` (clippy across all four feature flags). Extend these in preference to writing more prose.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
