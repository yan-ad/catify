# Configuration

Catify resolves configuration in this order, with each later source
overriding values from earlier sources:

1. Built-in defaults.
2. User configuration.
3. Project configuration.
4. Runtime overrides supplied by flags or the command environment.

Missing user and project files are optional. Other read failures, including
permission errors, are surfaced as configuration errors rather than silently
falling back to defaults. Invalid TOML diagnostics retain the source path and
one-based line and column when the parser provides a span.

The current schema is intentionally small:

```toml
[telemetry]
enabled = false
```

Unknown fields are rejected so misspelled settings do not appear to work.

## Filesystem guarantees

Configuration writes use a temporary sibling file, flush complete contents,
and replace the destination only after the temporary file is ready. Reported
failures clean up the temporary file and do not truncate the existing
destination. On Windows, replacement uses the operating system's atomic file
replacement API because the Rust standard rename operation cannot overwrite an
existing file.

Paths are normalized lexically without requiring them to exist. `.` segments
are removed, safe `..` segments are collapsed, and parent traversal above a
root is discarded. Native Unix roots, Windows drive prefixes, and UNC prefixes
are covered by the CI operating-system matrix.

## Project discovery and environments

Commands discover the nearest ancestor containing a Shopify project marker:

- App projects use `shopify.app.toml` or `shopify.app.<name>.toml`.
- Theme projects use `shopify.theme.toml`.

Invocation from nested directories resolves to the nearest matching project.
Directories merely named like marker files are ignored. Auto-discovery rejects
a root containing both app and theme markers; domain-specific commands must
select the expected project type rather than silently choosing one.

For app projects, `shopify.app.toml` is the default configuration when present.
If a project has only named variants, multiple variants require an explicit
`--config <name>` selection. Errors list the available names. The configuration
may also be selected with `CFY_CONFIG` or the Shopify-compatible
`SHOPIFY_FLAG_APP_CONFIG` environment variable.

Effective project values use this precedence, from highest to lowest:

1. Explicit command flags.
2. `CFY_STORE` / `CFY_ORGANIZATION`, followed by compatible
   `SHOPIFY_FLAG_STORE` / `SHOPIFY_FLAG_ORGANIZATION` values.
3. `store`, `organization`, or `organization_id` in the selected project TOML.

Environment maps are supplied explicitly to the resolver instead of being read
from global process state. This keeps concurrent commands and tests
deterministic and lets the CLI sanitize inherited environment variables at its
boundary.
