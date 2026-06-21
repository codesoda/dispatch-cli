# dispatch result

Report task completion.

## As an agent

```sh
dispatch result --message-id <id> --status done --summary "<what you did>"
```

Records the outcome of the task you received in `<id>`. Identity comes from
`$DISPATCH_WORKER_ID`.

Flags:

- `--status <done|failed|blocked>` — required.
- `--summary "<text>"` — optional free-text summary of what happened.
- `--artifact <path-or-url>` — optional, repeatable; outputs you produced.
- `--for-agent` — print a terse confirmation instead of the JSON envelope.

Examples:

```sh
dispatch result --message-id m1 --status done --summary "fixed the flaky test" --artifact out/report.md
dispatch result --message-id m2 --status blocked --summary "need the staging DB creds"
```

## A "super-ack"

`result` rides the same machinery as `ack`: it validates the same way (the
message must exist and be addressed to you) and records the acknowledgement —
**plus** the status, summary, and artifacts. So you do **not** need to `ack`
before `result`; one `result` call both acknowledges and completes.

## The loop contract

`result` contains no loop logic. After you call it, your turn ends. The stop
hook brings you back to `listen` for the next task (see `the-listen-loop.md`).
Do not stop on your own.
