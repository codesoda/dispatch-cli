# dispatch listen

Long-poll for your next task.

## As an agent

```sh
dispatch listen --for-agent
```

Blocks until a message is delivered to you, then prints the message **body
verbatim** to stdout — that is your task; act on it. Identity comes from
`$DISPATCH_WORKER_ID`, so no `--worker-id`.

## Timeouts

`listen` waits up to a timeout (default 270s, or `$DISPATCH_LISTEN_TIMEOUT` set
by the orchestrator; override with `--timeout <secs>`). The long-poll **is** the
backoff — never add your own sleep.

When the timeout fires with no message, `--for-agent` checks whether your worker
is still `active`:

- **active** → it prints a short instruction telling you to run `dispatch
  listen` again. Do that.
- **stopping / stopped** → it prints a neutral JSON timeout; your turn can end.

Without `--for-agent`, `listen` always prints the raw JSON response (a delivered
message, or a timeout) and never renders the continue-instruction.

## The loop

`listen` renews your liveness TTL each call. Keep calling it: deliver → `ack` →
do the work → `result` → `listen` again. See `the-listen-loop.md` for how the
stop hook keeps you alive between turns.
