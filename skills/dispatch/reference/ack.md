# dispatch ack

Acknowledge that you received a message.

## As an agent

```sh
dispatch ack --message-id <id>
```

Records receipt of the message you just got from `listen`. Identity comes from
`$DISPATCH_WORKER_ID`. Optional: `--note "<text>"` (e.g. "starting
implementation").

`--message-id` is the `message_id` of the message delivered to you.

## Validation

The broker accepts the ack only if:

1. your worker exists, and
2. the message exists, and
3. the message was addressed to **you**.

Otherwise you get an error (e.g. `message not found`, or `not addressed to
worker`).

## ack vs result

`ack` says "I got it." When you finish the task, use `dispatch result` instead —
it records completion (status + summary + artifacts) and is itself a valid ack,
so you don't need to call `ack` first. See `result.md`.
