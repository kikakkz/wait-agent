# Decisions

Architecture and process decisions, one file per decision. Each record is
immutable except status flips; superseded decisions keep their file with
status `superseded` and a pointer to the replacement.

| AD | Status | Decision |
|----|--------|----------|
| [AD-0001](AD-0001-issue-driven-task-source.md) | accepted | Task source moves from the local `.agents/tasks` board to GitHub issues |
| [AD-0002](AD-0002-relay-phase-1-design.md) | accepted | Relay phase 1: self-hosted CS star topology per docs/relay-design.md |
