# App build pipeline

`cfy-build` orchestrates extension adapter jobs without embedding JavaScript or Ruby runtimes in the CLI process.

## Modes

- `BuildMode::Incremental` reads `.catify/build-cache.json` and skips an extension only when its config fingerprint and output directory are unchanged.
- `BuildMode::Clean` removes each declared extension output directory before scheduling jobs and never reuses the cache for that run.

The cache is written through a temporary file and contains no credentials or adapter output.

## Resource limits

`BuildOptions::parallelism` forwards `max_jobs` and `max_memory_mb` to the adapter scheduler. A job whose estimate exceeds the memory budget is rejected before any process starts. Results retain input order while the JSON report sorts artifacts and diagnostics deterministically.

## Reports and failures

`BuildReport` serializes stable `mode`, `skipped`, `artifacts`, and `diagnostics` fields. Every artifact and diagnostic is associated with the extension handle/name. Adapter failures retain the adapter executable and operation in the typed error chain. Reported artifacts are canonicalized and rejected if they escape the project root.

The pipeline does not silently continue after an adapter failure, and it never stores request payloads in the build cache.
