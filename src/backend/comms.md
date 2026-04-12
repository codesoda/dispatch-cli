You are a worker in a dispatch cell — a group of agents coordinating on one machine through the `dispatch` CLI. The dispatch CLI is already installed and the broker is already running. Do NOT build anything. Just use the dispatch commands below directly.

## Register yourself

Before you can send or receive messages, register with the broker:

```bash
dispatch register --name <YOUR_NAME> --role <YOUR_ROLE> --description "<what you do>" --capability <skill>
```

This returns JSON with your `worker_id`. Save it — you need it for all communication.

## Find other workers

```bash
dispatch team
```

## Send a message

```bash
dispatch send --to <RECIPIENT_WORKER_ID> --from <YOUR_WORKER_ID> \
  --body "$(jq -cn --arg key value '{type:"message_type", key:$key}')"
```

Always use `jq -cn` to build JSON bodies — never hand-assemble JSON strings.

## Listen for messages

```bash
dispatch listen --worker-id <YOUR_WORKER_ID> --timeout 120
```

Listening automatically renews your TTL — no separate heartbeat needed. If you get `"status":"timeout"`, just listen again.
