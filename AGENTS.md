# Working Agreements

## Code

- Optimize for the reader. Code should be obvious on the first pass and easy to scan later.
- Prefer explicit data flow, small cohesive modules, descriptive names, and early returns.
- Prefer associated functions when behavior naturally belongs to a type. Use free functions for
  module-level operations that have no clear owner.
- Import types into scope instead of qualifying their full paths at use sites. Alias imports when
  names conflict.
- Keep parsing and presentation at the boundary. Core types should not depend on CLI or terminal concerns unless that removes meaningful duplication.
- Keep visibility as narrow as possible. Prefer private items, then `pub(crate)`; add public APIs only when another crate needs them.
- Avoid clever abstractions, hidden control flow, premature generalization, and tiny pass-through helpers.
- Remove meaningful repetition with shared functions or test harnesses, but do not abstract one-line operations merely to be DRY.
- Model distinct states with enums and typed structures instead of loosely related booleans or strings.
- Use typed errors with useful context. Preserve error sources and identify relevant paths or operations.
- Add dependencies only when they make the implementation materially smaller, safer, or clearer.

## Secrets

- Secret-owning application types must implement `Zeroize` and zeroize on drop.
- Secret wrappers must not implement `Clone`, `Display`, or serialization. Their `Debug` output must be redacted.
- Secret access must be explicit and narrowly scoped. Never include credentials in logs, errors, status output, configuration rendering, or tests.
- Document ownership boundaries when a dependency retains a non-zeroizing copy; do not claim guarantees beyond memory owned by this crate.

## Documentation

- Comments explain non-obvious intent, ownership, invariants, and tradeoffs.
- Do not narrate transformations or control flow that the code already makes clear.
- Write documentation for readers who are new to the repository but understand the project's purpose.
- Keep documentation close to the behavior it describes and update it with behavioral changes.

## Testing

- Test externally meaningful behavior and important failure modes, not private implementation details.
- Keep tests readable and deterministic. Introduce fixtures or harnesses when they remove substantial repetition.
- For regressions, add the failing regression test in one revision, implement the fix in a child revision, verify it, then squash the fix into the test revision.
- For new features, keep their tests in the same meaningful revision as the feature.
