# Vendored skills

`replica-*` come from https://github.com/Jakeschincariol/replica-skill
at commit 77c9436fb3d18c3d58169efb8caf4fe906b0dc51 (MIT, see LICENSE.replica-skill).
Their Python tools use only the standard library (no network, no subprocess); checked 2026-11-08.

Add your own skill as a folder holding a SKILL.md with `name:` and `description:` frontmatter.
`senapati init` copies this folder into `<home>\skills`.
