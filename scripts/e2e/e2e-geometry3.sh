#!/bin/bash
# geometry-3 e2e: authority coordinator on one host, isolated ports.
# A (server, 17474) mirrors B (client node daemon, 17475).
# Verifies: detached resize, small-client attach (kk-wins noop),
# big-client attach (padding panes), detach restore.
set -u
BIN=/root/wait-agent/target/debug/waitagent
DRV=/root/.local/share/waitagent/tmux
DRVSOCK=wa-e2e3-driver
APORT=17474
BPORT=17475
LOG=/tmp/waitagent-diag.log

step() { echo; echo "== $* =="; }
bpanes() { $DRV -L "$BSOCK" list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height} #{pane_title}' 2>/dev/null | grep "^${BSESS}"; }
bwin() { $DRV -L "$BSOCK" list-windows -F '#{window_index} #{window_width}x#{window_height} panes=#{window_panes}'; }

pkill -f "[w]aitagent --port ${APORT}" 2>/dev/null
pkill -f "[w]aitagent --port ${BPORT}" 2>/dev/null
$DRV -L $DRVSOCK kill-server 2>/dev/null
sleep 1

step "start A + B"
$DRV -L $DRVSOCK new-session -d -s drv -x 200 -y 50 "$BIN --port ${APORT} --public 127.0.0.1:${APORT}"
sleep 4
nohup "$BIN" --port ${BPORT} --connect 127.0.0.1:${APORT} --node-id 127.0.0.1#${BPORT} __remote-daemon >/tmp/wa-e2e3-b.log 2>&1 &
sleep 6
TARGET=$(grep remote_runtime_owner_upsert $LOG | grep "127.0.0.1#${BPORT}" | tail -1 | sed -n 's/.*target=\([^ ]*\).*/\1/p')
BSOCK=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${BPORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
echo "target=$TARGET Bsock=$BSOCK"
BSESS=${BSOCK#wa-}
ASOCK=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
ASESS=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "session-name [0-9a-f]*" | awk '{print $2}' | head -1)
"$BIN" --port ${APORT} --public 127.0.0.1:${APORT} __activate-target --current-socket-name "$ASOCK" --current-session-name "$ASESS" --target "$TARGET"
sleep 6

step "1) DETACHED: expect B window ~200x49, main 167x47, sidebar 32x47, footer 200x1, no padding"
bwin; bpanes
grep "send_resize_applied" $LOG | tail -2
grep "send_target_geometry_changed" $LOG | tail -2

step "2) attach SMALL client (80x24) to B session: expect window 80x24, main 47x22, chrome restored, push 47x22"
$DRV -L $DRVSOCK new-session -d -s c1 -x 80 -y 24 "$DRV -L $BSOCK attach-session -t $BSESS"
sleep 5
bwin; bpanes
grep "send_target_geometry_changed" $LOG | tail -2
grep "target geometry changed" $LOG | tail -2
$DRV -L $DRVSOCK kill-session -t c1
sleep 1

step "3) attach BIG client (237x60): expect main 167x47 + hpad 36 + vpad 10, sidebar x=205, footer bottom"
$DRV -L $DRVSOCK new-session -d -s c2 -x 237 -y 60 "$DRV -L $BSOCK attach-session -t $BSESS"
sleep 5
bwin; bpanes
$DRV -L "$BSOCK" display-message -p -t 0 '#{window_layout}'
grep "send_target_geometry_changed" $LOG | tail -2
$DRV -L $DRVSOCK kill-session -t c2

step "4) DETACH all: expect window 200x49, main 167x47, padding killed"
sleep 5
bwin; bpanes
grep "send_target_geometry_changed" $LOG | tail -3

step "done; leaving instances for inspection"
echo "cleanup: pkill -f 'waitagent --port ${APORT}'; pkill -f 'waitagent --port ${BPORT}'; $DRV -L $DRVSOCK kill-server"
