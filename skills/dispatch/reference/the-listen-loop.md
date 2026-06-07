# The listen loop

How dispatch keeps you alive between turns — and why you must not stop on your
own.

## The cycle

```
register --for-agent      (once, at boot — fetch your role prompt)
   │
   ▼
listen --for-agent  ◄────────────────┐
   │  (a message arrives)            │
   ▼                                 │
do the work                          │
   │                                 │
   ▼                                 │
ack  →  result                       │
   │                                 │
   └─── your turn ends ──────────────┘
        (the stop hook re-arms listen)
```

## Why you don't stop

Your vendor CLI (claude/codex) wants to stop when a turn's work is done. A
**stop hook** intercepts that and asks dispatch: *is this worker still
supposed to be running?*

- Your worker is **`active`** → the hook **blocks the stop** and hands you a
  continue-instruction (the "run `dispatch listen` again" text). Keep going.
- Your worker is **`stopping`/`stopped`**, or dispatch is unreachable → the hook
  **allows the stop**. You're done.

The coordinator owns that decision by setting your control state. So: **don't
decide to stop because you think you've finished everything.** Finish the
current task, `result` it, and return to `listen`. Stop only when dispatch
stops you (or `listen --for-agent` stops telling you to listen again).

## If there's no task right now

A `listen` timeout while you're still `active` prints something like:

```text
No task right now. Run `dispatch listen` again and keep waiting.
Do not stop until dispatch tells you to.
```

Follow it: run `dispatch listen --for-agent` again. The long-poll is the
backoff — never insert your own sleep.
