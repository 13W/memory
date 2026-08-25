---
schema_version: "1.1"
name: recall-before-read
description: In a clean first stretch, does the agent consult durable memory before it starts reading files?
tags: [adoption, f1]
plugins: [memory]
runs: 10
max_turns: 12
timeout_seconds: 300
---

The `retention` sweep in this repository takes a lock it does not need. Find where that
happens and explain, in two sentences, why the current transaction type is wrong.
