# Extension build adapter protocol

`cfy-extension-adapter` defines the boundary between Catify and external extension build tools. The boundary is deliberately versioned and machine-readable so extension-specific toolchains do not become hard-coded CLI behavior.

Theme build and development behavior is not part of this protocol.

## Protocol version 1

An adapter is an executable. Catify discovers it by an absolute/relative path or by searching `PATH`, then runs it with `--cfy-adapter-info`. The executable must write exactly one JSON value to stdout:

```json
{
  "protocol_version": 1,
  "name": "acme-ui-builder",
  "adapter_version": "1.4.0",
  "extension_types": ["ui_extension"]
}
```

`adapter_version` must be SemVer. Callers may impose a SemVer requirement. Missing executables, malformed versions, unsupported protocol versions, and unsatisfied requirements produce errors that identify the adapter and suggest installation/configuration remediation.

To build, Catify runs the same executable with `--cfy-build-adapter`. The JSON request is supplied as one document on standard input. This avoids environment-size limits and keeps build configuration out of child-process environment listings:

```json
{
  "protocol_version": 1,
  "extension_type": "ui_extension",
  "extension_dir": "/app/extensions/example",
  "output_dir": "/app/.cfy/build/example",
  "configuration": null
}
```

The adapter writes a response to stdout:

```json
{
  "protocol_version": 1,
  "artifacts": ["dist/main.js"],
  "diagnostics": [
    { "level": "warning", "message": "example warning" }
  ]
}
```

Human-readable progress belongs on stderr. Stdout is reserved for the JSON response. Paths in `artifacts` are interpreted by the caller relative to the extension/output context it supplied.

## Execution and cancellation

Discovery and every build are launched through `cfy-process::Supervisor`. Adapters therefore receive the same process-tree tracking, cancellation, signal forwarding, grace period, and forced cleanup as other Catify child processes. Do not launch adapters directly with `std::process::Command` or `tokio::process::Command`.

## Parallel builds and memory

`build_all` accepts two explicit limits:

- `max_jobs`: maximum concurrently running adapters.
- `max_memory_mb`: total estimated memory admitted concurrently.

Each `BuildJob` supplies a positive `memory_mb` estimate. A job larger than the total budget is rejected before anything starts, with guidance to increase the budget or lower the estimate. Two semaphores enforce both limits; a job starts only after it owns one job slot and its requested memory permits. Responses retain input order.

This is admission control, not an operating-system hard memory cap. Adapter integrations should use conservative measured estimates.
