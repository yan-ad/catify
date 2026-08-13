# Configuration

Crabpify resolves configuration in this order, with each later source
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
destination. On Windows, replacement uses a backup and rollback because the
standard rename operation cannot overwrite an existing file.

Paths are normalized lexically without requiring them to exist. `.` segments
are removed, safe `..` segments are collapsed, and parent traversal above a
root is discarded. Native Unix roots, Windows drive prefixes, and UNC prefixes
are covered by the CI operating-system matrix.
