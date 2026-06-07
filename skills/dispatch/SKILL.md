---
name: dispatch
description: >-
  Operate as a dispatch worker — an agent coordinated by the dispatch CLI. Use
  when your identity is set in $DISPATCH_WORKER_ID, or when asked to register,
  listen for tasks, acknowledge messages, or report results via `dispatch`.
  Covers the register → listen → work → result loop and why you must not stop on
  your own.
---

# dispatch — agent quickstart

You are a **dispatch worker**. Your identity is already in `$DISPATCH_WORKER_ID`
(the orchestrator set it), so every command below is bare — you never pass
`--worker-id`.

## The loop

1. **Register & get your role:** `dispatch register --for-agent`
   The response body (on stdout) is your operating prompt — follow it.
2. **Get your next task:** `dispatch listen --for-agent`
   Blocks until a message arrives, then prints it. On timeout it tells you to
   listen again.
3. **Acknowledge receipt:** `dispatch ack --message-id <id>`
4. **Report completion:** `dispatch result --message-id <id> --status done --summary "<what you did>"`
   Status is one of `done` | `failed` | `blocked`.

After step 4 your turn ends and dispatch brings you back to `listen` for the
next task.

## Do not stop on your own

You run in a loop held open by dispatch. When your turn ends, a stop hook asks
dispatch whether you should keep going; while your worker is `active` it tells
you to `dispatch listen` again. **Keep listening until dispatch tells you to
stop** — don't decide to exit because you think you're "done"; there may be more
work queued. If `listen` returns a "no task right now" instruction, run
`dispatch listen` again.

## More detail, on demand

Each verb has a full reference page — read only the one you need:

- `reference/register.md` — claiming your worker, `--for-agent`, identity
- `reference/listen.md` — long-poll, `--timeout`, what timeouts mean
- `reference/ack.md` — acknowledging a message
- `reference/result.md` — reporting completion (a "super-ack")
- `reference/the-listen-loop.md` — how the stop hook keeps you alive
