# Dispatch Communication Guide

You are a worker in a dispatch cell — a group of agents coordinating on one machine through the `dispatch` CLI. This guide explains how to register, send messages, and listen for messages.

**IMPORTANT: `dispatch` is already installed and the broker is already running. Do NOT build anything. Do NOT run cargo. Just use the `dispatch` commands below directly.**

## Register yourself

Before you can send or receive messages, register with the broker:

```bash
dispatch register --name <YOUR_NAME> --role <YOUR_ROLE> \
  --description "<what you do>" \
  --capability <skill1> --capability <skill2>
```

This returns JSON with your `worker_id`. Save it — you need it for all communication.

## Find other workers

```bash
dispatch team
```

Returns a JSON list of all active workers with their `id`, `name`, `role`, and `capabilities`. Use this to find the worker you need to talk to.

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

Always send a heartbeat before listening. Workers expire after 5 minutes without a heartbeat. The listen command long-polls — it blocks until a message arrives or the timeout is reached.

If you get `"status":"timeout"`, just heartbeat and listen again.

## Message format

All message bodies should be valid JSON with a `type` field so the recipient knows how to handle it. For example:

```json
{"type": "request", "topic": "...", "version": 1}
{"type": "response", "result": "...", "status": "ok"}
```

## Tips

- Run `dispatch heartbeat` before every `dispatch listen`
- The other agents are Claude Code instances in separate terminals — give them time to respond
- Show the user every message you send and receive so they can follow the collaboration
