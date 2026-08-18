# App development orchestration

`cfy-dev` owns the lifecycle state machine for app development components. It does not reimplement process supervision: child process groups, output draining, cancellation, and forced cleanup are delegated to `cfy-process`.

The lifecycle is explicit:

```text
Pending -> Starting -> Ready -> Running -> Stopped
                                  |
                                  v
                            Restarting -> Running
                                  |
                                  v
                               Failed
```

A component failure exhausts its restart budget, emits a failure event, and shuts down sibling processes. Cancellation follows the same cleanup path. `max_parallel` is validated at this boundary; build/extension concurrency remains owned by `cfy-build`, where the memory budget is enforceable.

The event stream is structured and contains component-scoped output, state transitions, and recovery diagnostics. This keeps CLI rendering separate from process lifecycle behavior.
