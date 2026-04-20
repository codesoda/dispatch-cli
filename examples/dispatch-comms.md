# Dispatch Communication Guide

You are a worker in a dispatch cell — a group of agents coordinating on one machine through the `dispatch` CLI. This guide explains how to register, communicate, and report status.

**IMPORTANT: `dispatch` is already installed and the broker is already running. Do NOT build anything. Do NOT run cargo. Just use the `dispatch` commands below directly.**

## The listen loop

You are a long-lived worker, not a one-shot command. After you finish handling a message:

1. Acknowledge the message you just handled with `dispatch ack --message-id <id>` (or include `--note "..."` with a short result). Silence reads as failure to the sender — always ack as soon as you start handling a message, and again when you complete it if the result is meaningful.
2. Update your status with `dispatch heartbeat --status "..."`
3. Call `dispatch listen --worker-id <YOUR_WORKER_ID> --timeout 600` again
4. Handle the next message that arrives (or the timeout, then loop)

**Do not exit after processing a single message.** If the broker has nothing for you right now, `listen` will return `{"status":"timeout"}` after the timeout — that's normal; call `listen` again.

A vendor Stop hook may also remind you to keep listening by injecting a "do not stop" instruction. Behave correctly without relying on the hook — it's a safety net, not the primary mechanism.

## Register yourself

Before you can send or receive messages, register with the broker:

```bash
dispatch register --name <YOUR_NAME> --role <YOUR_ROLE> \
  --description "<what you do>" \
  --capability <skill1> --capability <skill2> --evict
```

This returns JSON with your `worker_id`. Save it — you need it for all communication. The `--evict` flag ensures any stale registration with the same name is replaced.

## Find other workers

```bash
dispatch team
```

Returns a JSON list of all active workers with their `id`, `name`, `role`, `capabilities`, and `last_status`. Use this to find the worker you need to talk to.

## Send a message

```bash
dispatch send --to <RECIPIENT_WORKER_ID> --from <YOUR_WORKER_ID> \
  --body "$(jq -cn --arg key value '{type:"message_type", key:$key}')"
```

Always use `jq -cn` to build the JSON body — never hand-assemble JSON strings.

## Listen for messages

```bash
dispatch heartbeat --worker-id <YOUR_WORKER_ID>
dispatch listen --worker-id <YOUR_WORKER_ID> --timeout 120
```

Sending a heartbeat before listening is a safe default. Workers expire after 1 hour without a heartbeat or other broker activity — `listen`, `send`, and `team` also renew the TTL — so a separate heartbeat may be unnecessary while you're actively listening (configurable via `--ttl` on register or `default_ttl` in config). The listen command long-polls — it blocks until a message arrives or the timeout is reached.

If you get `"status":"timeout"`, just heartbeat and listen again.

## Acknowledge messages

When you receive a message and start working on it, acknowledge it so the sender (and the monitoring dashboard) knows you picked it up:

```bash
dispatch ack --worker-id <YOUR_WORKER_ID> \
  --message-id <MESSAGE_ID> --note "starting implementation"
```

The `--note` is optional but useful for context.

## Report your status

While working on a task, update your status so the team knows what you're doing:

```bash
dispatch heartbeat --worker-id <YOUR_WORKER_ID> \
  --status "implementing login flow 2/5"
```

Status is a short tagline visible in `dispatch team`, `dispatch status`, and the monitor dashboard. Update it as your work progresses.

## Check team status

```bash
dispatch status                              # all workers with their latest status
dispatch status --worker-id <WORKER_ID>      # one worker's status
```

## Inspect messages and events

```bash
dispatch messages --worker-id <YOUR_WORKER_ID>         # your inbox
dispatch messages --worker-id <YOUR_WORKER_ID> --sent   # messages you sent
dispatch events --since 5m                              # recent broker activity
dispatch events --type ack --since 10m                  # recent acks
```

These are non-destructive — they don't consume messages or affect state.

## Message format

All message bodies should be valid JSON with a `type` field so the recipient knows how to handle it. For example:

```json
{"type": "request", "topic": "...", "version": 1}
{"type": "response", "result": "...", "status": "ok"}
```

## Tips

- Run `dispatch heartbeat` before every `dispatch listen`
- Use `--status` on heartbeat to let the team know what you're doing
- Ack messages when you start handling them — silence looks like failure
- The other agents may be Claude Code, Codex, or plain worker scripts running in separate terminals — give them time to respond, and don't assume any specific reply latency or LLM-style answer format
- Show the user every message you send and receive so they can follow the collaboration
- Use `dispatch events --since 5m` to debug what happened if something seems lost
