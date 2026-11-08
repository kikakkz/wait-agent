# WaitAgent

At the start of every session, you must first read `.agents/index.yaml`, then load context according to the `read_order` in it.

All execution rules, current tasks, constraints, primitives, and workflows are defined in `.agents/`. This is the only machine control plane.

Do not maintain human documentation (`docs/`) unless the user explicitly requests it.
