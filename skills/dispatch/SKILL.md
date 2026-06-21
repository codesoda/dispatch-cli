---
name: dispatch
description: >-
  Operate as a dispatch worker coordinated by the dispatch CLI. Use when
  $DISPATCH_WORKER_ID is set, or when asked to register, listen, ack, or report
  results via `dispatch`. Covers the register → listen → work → result loop and
  why you must not stop on your own.
---

# dispatch — agent quickstart

Your identity is already in `$DISPATCH_WORKER_ID`, so every command is bare —
never pass `--worker-id`.

## The loop

1. **Register & get your role:** `dispatch register --for-agent`
   — stdout is your operating prompt; follow it.
2. **Get your next task:** `dispatch listen --for-agent`
   — blocks until a message arrives, then prints it; on timeout, tells you to listen again.
3. **Acknowledge receipt:** `dispatch ack --message-id <id>`
4. **Report completion:** `dispatch result --message-id <id> --status done --summary "<what you did>"`
   — status is `done` | `failed` | `blocked`.

Then your turn ends and dispatch returns you to `listen` for the next task.

## Don't stop on your own

A stop hook holds the loop open: while your worker is `active` it sends you back
to `dispatch listen`. Keep listening until dispatch stops you — don't exit just
because you think you're "done"; more work may be queued. A "no task right now"
timeout means: listen again.

## More detail, on demand

Read only the page you need:

- `reference/register.md` — claiming your worker, `--for-agent`, identity
- `reference/listen.md` — long-poll, `--timeout`, timeouts
- `reference/ack.md` — acknowledging a message
- `reference/result.md` — reporting completion (a "super-ack")
- `reference/the-listen-loop.md` — how the stop hook keeps you alive
