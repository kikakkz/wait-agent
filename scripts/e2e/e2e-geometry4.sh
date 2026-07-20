#!/bin/bash
# geometry-4 e2e: server-side re-sync on geometry push.
# A (server, 17474) mirrors B (client node daemon, 17475).
# When B's client attaches small, authority pushes T; A's slot must
# resize its own pane/window to T and re-bootstrap cleanly.
set -u
BIN=/root/wait-agent/target/debug/waitagent
DRV=/root/.local/share/waitagent/tmux
DRVSOCK=wa-e2e4-driver
APORT=17474
BPORT=17475
LOG=/tmp/waitagent-diag.log

step() { echo; echo "== $* =="; }

pkill -f "[w]aitagent --port ${APORT}" 2>/dev/null
pkill -f "[w]aitagent --port ${BPORT}" 2>/dev/null
$DRV -L $DRVSOCK kill-server 2>/dev/null
sleep 1

step "start A + B"
$DRV -L $DRVSOCK new-session -d -s drv -x 200 -y 50 "$BIN --port ${APORT} --public 127.0.0.1:${APORT}"
sleep 4
nohup "$BIN" --port ${BPORT} --connect 127.0.0.1:${APORT} --node-id 127.0.0.1#${BPORT} __remote-daemon >/tmp/wa-e2e4-b.log 2>&1 &
sleep 6
TARGET=$(grep remote_runtime_owner_upsert $LOG | grep "127.0.0.1#${BPORT}" | tail -1 | sed -n 's/.*target=\([^ ]*\).*/\1/p')
BSOCK=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${BPORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
BSESS=${BSOCK#wa-}
ASOCK=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | head -1)
ASESS=$(ps aux | grep "[_]ui-sidebar" | grep -- "--port ${APORT}" | grep -o "session-name [0-9a-f]*" | awk '{print $2}' | head -1)
echo "target=$TARGET Bsock=$BSOCK Asock=$ASOCK"
"$BIN" --port ${APORT} --public 127.0.0.1:${APORT} __activate-target --current-socket-name "$ASOCK" --current-session-name "$ASESS" --target "$TARGET"
sleep 6

SLOTPANE=$($DRV -L "$ASOCK" list-panes -a -F '#{pane_id} #{pane_current_command}' | grep waitagent | awk '{print $1}' | while read -r p; do pid=$($DRV -L "$ASOCK" list-panes -a -F '#{pane_id} #{pane_pid}' | grep "^$p" | awk '{print $2}'); if ps -p "$pid" -o cmd= 2>/dev/null | grep -q "__remote-main-slot"; then echo "$p"; fi; done | head -1)
echo "A slot pane=$SLOTPANE"
apanes() { $DRV -L "$ASOCK" list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height} #{pane_title}'; }
bslot() { $DRV -L "$ASOCK" capture-pane -p -t "$SLOTPANE" 2>/dev/null | tail -14; }

step "1) baseline: A slot pane should be 167x47; run a long-line command in the mirror"
apanes | grep -E "$(echo $SLOTPANE | tr -d '%') "
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" 'echo AAAAAAAA-BBBBBBBB-CCCCCCCC-DDDDDDDD-EEEEEEEE-FFFFFFFF-GGGGGGGG-HHHHHHHH-IIIIIIII-JJJJJJJJ-KKKKKKKK-LL' Enter
sleep 2
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" 'ls' Enter
sleep 2
bslot

step "2) Ctrl+R in the mirror at full size (baseline sanity)"
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" C-r
sleep 1
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" 'AAAA'
sleep 1
bslot
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" C-c
sleep 1

step "3) attach SMALL client (80x24) to B: expect A slot pane -> 47x22 and clean re-bootstrap"
$DRV -L $DRVSOCK new-session -d -s c1 -x 80 -y 24 "$DRV -L $BSOCK attach-session -t $BSESS"
sleep 6
apanes | grep -E "$(echo $SLOTPANE | tr -d '%') |padding"
bslot

step "4) Ctrl+R after re-sync: the reverse-i-search line must render cleanly (no fragments)"
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" C-r
sleep 1
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" 'AAAA'
sleep 1
bslot
$DRV -L "$ASOCK" send-keys -t "$SLOTPANE" C-c
sleep 1

step "5) detach small client: expect A slot pane back to 167x47, clean"
$DRV -L $DRVSOCK kill-session -t c1
sleep 6
apanes | grep -E "$(echo $SLOTPANE | tr -d '%') |padding"
bslot

step "done"
echo "cleanup: pkill -f 'waitagent --port ${APORT}'; pkill -f 'waitagent --port ${BPORT}'; $DRV -L $DRVSOCK kill-server"
