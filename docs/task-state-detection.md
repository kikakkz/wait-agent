# Task state detection design

How the console decides the sidebar task state of a managed session.

## States

| State      | String       | Meaning                                                                  | Badge     |
|------------|--------------|--------------------------------------------------------------------------|-----------|
| Running    | `running`    | The foreground process or agent is actively working.                     | 🔥R yellow |
| Input      | `input`      | The prompt is idle and accepts input.                                    | 🔊I cyan  |
| Confirm    | `confirm`    | The agent asks a question or for permission.                             | 📢C magenta |
| Background | `background` | The prompt is idle and accepts input while background task(s) are running. | 🧵B blue |
| Unknown    | `unknown`    | No inference is available.                                               | ·U gray   |

## Writers and convergence

`ManagedSessionRecord.task_state` has two independent writers:

- Text inference: the process monitor polls pane text (at most once per
  second) and derives a state through the per-agent detector
  (`AgentDetector::infer_task_state`).
- Hook signals: agent hook events map directly to states
  (`AgentDetector::signal_state_effect`).

Last writer wins. The writers disagree only inside short windows (a hook
fires while the pane still shows the previous turn's status line); the
poll runs at most one second behind, so the state converges to the
inference result quickly. Arbitration rules between the writers are
intentionally absent: every rule would have to be maintained per agent
and per hook event, while convergence only requires the detector to be
accurate.

## Kimi precedence

The `kimi` detector scores pane text in this order; the first match
wins:

1. Confirm dialogs (`?` choice menus, plan approval menus, shared
   confirm phrases).
2. Foreground animation (braille or moon spinner, compacting line) →
   Running.
3. `[N task running]` status marker plus an idle prompt → Background.
4. `[N task running]` without a visible prompt → Running.
5. Idle prompt → Input.
6. Default → Running.

The `[N task running]` marker alone never means the foreground is busy:
Kimi keeps the marker on its status line while the main loop is idle and
the prompt accepts input. Reporting Running there hid the interactive
prompt and raced every Input-setting hook event, which produced the
running/input flip-flop.

## Wire compatibility

`task_state` crosses node-to-node links as a string, but connections are
exact-version gated at handshake (`REMOTE_PROTOCOL_VERSION`), and every
producer and consumer ships in this single crate, so a new state value
never mixes an old parser with a new string.
