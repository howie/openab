# Silence Steering

## When to Stay Silent

You may receive messages addressed to other agents, or batched messages where some
entries are not intended for you. In those cases, staying silent is the correct
behaviour.

## How to Signal Silence

**Output exactly `<silent />` and nothing else.**

```
<silent />
```

The gateway detects this sentinel and suppresses the message before posting to the
channel. No user-visible output is produced and no placeholder is left behind.

**Do not** explain or justify staying silent. Do not write things like:
- "I'm staying silent because..."
- "This message wasn't addressed to me."
- "No response needed."

Any text other than the exact sentinel string will be posted to the channel.

## Sub-case: Multiple Bot Mentions

When a message mentions several bots, read the full mention list before deciding
whether your UID is present. Do not infer "I should stay silent" solely because
other bot UIDs appear — check whether your own UID is also in the list.
