# Instructions for Claude Code

## Language

Everything in the repository (code, comments, test messages, docs, commit
messages) is written in English. The only exception is the `it:` entries of
the locale files in `crates/vv-app/locales/`, which are the Italian UI
translation.

## Comment style

Hard rule: comments in the code must be **short** and written **only when
necessary** — never to explain obvious things or things that can already be
read from the code itself (well-chosen variable/function names are enough).

A comment is justified only when it explains a non-obvious *why*: a hidden
constraint, the reason for a non-obvious choice, a worked-around bug, a
behaviour that would surprise the reader. If the code stays clear without
it, the comment must not be written.

Avoid in particular:
- Multi-paragraph essay comments on a single line or function.
- Repeating in the comment what the code already says (the *what* instead
  of the *why*).
- Long preambles/historical context when a single terse line would do.

## Tests

Unit tests do NOT go inline in the source file: they live in
`crates/<crate>/src/tests/<same path as the source>.rs`, pulled in with
`#[cfg(test)] #[path = "tests/<file>.rs"] mod tests;`. Never add an inline
`mod tests { … }`. Details in the "Where unit tests live" section of
`README.md`.

## Task management (Vikunja)

This project's task list is kept on Vikunja. When a feature is completed:

1. Create the commit for the feature.
2. Mark the corresponding task as done on Vikunja.
3. Leave a comment on the completed task with the short hash of the related
   git commit (e.g. `849a7f8`).

## Build

When a build is requested, copy the freshly compiled version to
`~/Nextcloud/Share/Venturi-builds` with architecture and timestamp as the
name suffix.

E.g.:
```
export TIMESTAMP=$(date +%s)
cp target/appimage/Venturi-x86_64.AppImage ~/Nextcloud/Share/Venturi-builds/vv-app-$(uname -m)-${TIMESTAMP}.AppImage
cp target/release/vv-app ~/Nextcloud/Share/Venturi-builds/vv-app-$(uname -m)-${TIMESTAMP}
```
