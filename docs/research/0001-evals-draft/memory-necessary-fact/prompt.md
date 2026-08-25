---
schema_version: "1.1"
name: memory-necessary-fact
description: An outcome metric — the task is only answerable correctly from a fact that exists in durable memory and nowhere in the working tree.
tags: [adoption, outcome]
plugins: [memory]
runs: 10
max_turns: 10
timeout_seconds: 300
---

Before you change anything: state the exact build command this repository requires,
including the toolchain version constraint, and say what happens if it is ignored.
