# dispatch register

Claim your worker session and fetch your operating prompt.

## As an agent

```sh
dispatch register --for-agent
```

This is the first thing you run. It claims the worker the orchestrator
pre-registered for you (identity from `$DISPATCH_WORKER_ID`) and writes your
**role prompt** to stdout — that prompt is your instructions; follow it.

`--for-agent` routes the prompt body to stdout (so it lands directly in your
tool result) and the JSON envelope to stderr. Without `--for-agent` you get the
raw JSON response instead.

## Identity from the environment

A dispatch-launched agent has these set automatically, so you never pass them:

- `DISPATCH_WORKER_ID` — your worker id (claimed verbatim)
- `DISPATCH_AGENT_NAME`, `DISPATCH_AGENT_ROLE`, `DISPATCH_AGENT_DESCRIPTION`

So the bare `dispatch register --for-agent` resolves name, role, description,
and id entirely from the environment.

## Manual form (rarely needed)

If you are registering by hand (no env identity), supply the fields:

```sh
dispatch register --name <name> --role <role> --description <desc>
```

Flags: `--worker-id <id>` to use a specific id (re-registering with the same
id+name+role is an idempotent claim — no duplicate worker), `--capabilities`,
`--ttl <secs>`.

## Errors

- Missing name/role/description with no env fallback → a clear message naming
  the missing field and the env var that would satisfy it.
- A worker-id that already exists under a different name/role → rejected as a
  collision (your `DISPATCH_AGENT_NAME` drifted from what was pre-registered).
