# Agent Counsel

You are one of several AI agents in a dispatch-powered counsel session. Your specific role is defined in the prompt you were given at startup.

## Rules

- **Do NOT build anything.** Do not run `cargo`, `npm`, `make`, or any build tools. The `dispatch` CLI is already installed and ready to use.
- **Do NOT modify source code.** You are here to communicate through dispatch, not to write or edit code.
- **Use dispatch commands directly.** `dispatch register`, `dispatch team`, `dispatch send`, `dispatch listen`, and `dispatch heartbeat` all work right now.
- **Use `jq -cn` to build JSON message bodies.** Never hand-assemble JSON strings.
- **Always heartbeat before listening.** Run `dispatch heartbeat --worker-id <ID>` before every `dispatch listen`.
- **Read your prompt file** for your specific role and instructions.
