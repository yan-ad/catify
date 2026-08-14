# Process supervision

`cfy-process` owns subprocess lifecycle management for compatibility adapters,
development servers, tunnels, and build tools. Callers should use `Supervisor`
instead of spawning long-running children directly.

## Lifecycle contract

- Every child is placed in an isolated process tree.
- Unix uses a process group. Windows uses a Job Object with
  `KILL_ON_JOB_CLOSE`.
- `RunningProcess::wait_with_signal_forwarding` forwards Ctrl-C to the child
  tree. Unix forwards `SIGINT`; Windows terminates the Job Object because
  console control events cannot be forwarded reliably from every host.
- `RunningProcess::cancel` and `Supervisor::shutdown` request termination and
  wait for the configured grace period before force-killing the process tree.
- Dropping a running handle requests cleanup. Dropping the platform process
  tree also kills descendants that outlive their direct parent.
- The direct child's exact `ExitStatus` is returned to the caller. Cancellation
  is reported separately through `ProcessOutput::cancelled`.

## Output modes

| Mode | Captured in `ProcessOutput` | Streamed as `OutputChunk` | Terminal inherited |
| --- | --- | --- | --- |
| `Capture` | yes | no | no |
| `Stream` | no | yes | no |
| `CaptureAndStream` | yes | yes | no |
| `Inherit` | no | no | yes |

Stdout and stderr are always represented as separate channels. Pipe readers run
concurrently so a noisy child cannot deadlock while the supervisor waits for it.
Stream events use an unbounded internal queue; callers handling untrusted or
unbounded output should continuously consume `next_output` or choose `Capture`
and impose an application-level output policy.

## Multi-child shutdown

One `Supervisor` can own multiple children. `shutdown` broadcasts cancellation
to every active child and returns only after the registry is empty. Integration
tests cover concurrent children, output capture and streaming, non-zero exit
codes, cancellation, and descendant cleanup on each CI operating system.
