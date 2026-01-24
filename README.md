# Dossiers

> [!CAUTION]  
> Dossiers is currently in active development. Features and APIs may change.

The Dossiers CLI is a command-line tool for turning a specification
repository into a static, navigable website.

It takes a directory of Markdown (or AsciiDoc, preliminary support only)
files and produces a fully rendered site with structured navigation and
search, making it easy to publish internal documentation, specifications,
or policies without running a full-blown server.

The CLI can be used on its own to generate static sites, or alongside
Dossiers to preview documentation locally in development workflows.

Typical use cases:

- Preview documentation changes locally before merging
- Generate static documentation sites for internal or offline use
- Publish specs, process docs, or policies from a Git repository
- Integrate documentation builds into CI pipelines

## Mermaid diagrams

Markdown fences using `mermaid` (and AsciiDoc source blocks such as
`[source,mermaid]`) are rendered client-side with Mermaid.js. The runtime is
vendored locally so it can be hosted alongside the generated site.

To update the Mermaid runtime:

```sh
./scripts/update-mermaid.sh <version>
```

This refreshes `assets/mermaid.min.js` and records the version in
`assets/mermaid.version`.

For more information, visit the Dossiers website at
https://dossie.rs
