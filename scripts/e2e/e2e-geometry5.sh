#!/bin/bash
# geometry-5 e2e: per-server geometry store.
# A (server, 17474) mirrors B (client node daemon, 17475, --connect 127.0.0.1:17474).
# After negotiation, the store must hold T for key "127.0.0.1:17474";
# a restarted B must create its session at the stored size, and mirror
# open must apply the stored geometry first.
set -u
BIN=/root/wait-agent/target/debug/waitagent
DRV=/root/.local/share/waitagent/tmux
DRVSOCK=wa-e2e5-driver
APORT=17474
BPORT=17475
LOG=/tmp/waitagent-diag.log
STORE=/root/.local/share/waitagent/per-server-geometry.json

step() { echo; echo "== $* =="; }
rm -f "$STORE"
pkill -f "[w]aitagent --port ${APORT}" 2>/dev/null
pkill -f "[w]aitagent --port ${BPORT}" 2>/dev/null
$DRV -L $DRVSOCK kill-server 2>/dev/null
sleep 1

step "start A + B, attach, negotiate"
$DRV -L $DRVSOCK new-session -d -s drv -x 200 -y 50 "$BIN --port ${APORT} --public 127.0.0.1:${APORT}"
sleep 4
nohup "$BIN" --port ${BPORT} --connect 127.0.0.1:${APORT} --node-id 127.0.0.1#${BPORT} __remote-daemon >/tmp/wa-e2e5-b.log 2>&1 &
sleep 6
TARGET=$(grep remote_runtime_owner_upsert $LOG | grep "127.0.0.1#${BPORT}" | tail -1 | sed -n 's/.*target=\([^ ]*\).*/\1/p')
ASOCK=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
ASESS=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "session-name [0-9a-f]*" | awk '{print $2}' | head -1)
"$BIN" --port ${APORT} --public 127.0.0.1:${APORT} __activate-target --current-socket-name "$ASOCK" --current-session-name "$ASESS" --target "$TARGET"
sleep 6

step "store file after negotiation (expect cols=167 rows=47 for key 127.0.0.1:17474)"
cat "$STORE" 2>&1

step "restart B: new session must be created at stored size, not 80x24"
pkill -f "[w]aitagent --port ${BPORT}"; sleep 3
nohup "$BIN" --port ${BPORT} --connect 127.0.0.1:${APORT} --node-id 127.0.0.1#${BPORT} __remote-daemon >/tmp/wa-e2e5-b2.log 2>&1 &
sleep 6
BSOCK2=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${BPORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
echo "B new socket=$BSOCK2"
$DRV -L "$BSOCK2" list-windows -F '#{window_index} #{window_width}x#{window_height}'
$DRV -L "$BSOCK2" list-panes -a -F '#{window_index}.#{pane_index} #{pane_width}x#{pane_height} #{pane_title}'

step "re-attach from A: mirror open should apply stored geometry first"
grep "applying stored geometry" $LOG | tail -2
TARGET2=$(grep remote_runtime_owner_upsert $LOG | grep "127.0.0.1#${BPORT}" | tail -1 | sed -n 's/.*target=\([^ ]*\).*/\1/p')
echo "target2=$TARGET2"
"$BIN" --port ${APORT} --public 127.0.0.1:${APORT} __activate-target --current-socket-name "$ASOCK" --current-session-name "$ASESS" --target "$TARGET2"
sleep 5
grep "applying stored geometry" $LOG | tail -2
grep "send_resize_applied" $LOG | tail -2

step "done"
echo "cleanup: pkill -f 'waitagent --port ${APORT}'; pkill -f 'waitagent --port ${BPORT}'; $DRV -L $DRVSOCK kill-server; rm -f $STORE"
