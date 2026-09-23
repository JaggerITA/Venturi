# Instructions for AI agents

The project rules for agents are in [CLAUDE.md](CLAUDE.md) and apply to
every agent, not just Claude: read it before making changes.

The one most likely to be missed:

## Tests

Unit tests do NOT go inline in the source file: they live in
`crates/<crate>/src/tests/<same path as the source>.rs`, pulled in with
`#[cfg(test)] #[path = "tests/<file>.rs"] mod tests;`. Never add an inline
`mod tests { … }`. Details in the "Where unit tests live" section of
`README.md`.
