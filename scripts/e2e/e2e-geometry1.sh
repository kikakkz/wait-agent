#!/bin/bash
# geometry-1 e2e: isolated two-instance check on this machine.
# Instance A (server, port 17474) + Instance B (client node daemon, port 17475).
# Must NOT touch the production instance on port 7474.
set -u
BIN=/root/wait-agent/target/debug/waitagent
DRV=/root/.local/share/waitagent/tmux
DRVSOCK=wa-e2e-driver
APORT=17474
BPORT=17475
LOG=/tmp/waitagent-diag.log

step() { echo; echo "== $* =="; }

# 0. clean any previous e2e leftovers (only ours, port-scoped)
pkill -f "waitagent --port ${APORT} --public 127.0.0.1:${APORT}" 2>/dev/null
pkill -f "waitagent --port ${BPORT} --connect 127.0.0.1:${APORT}" 2>/dev/null
$DRV -L $DRVSOCK kill-server 2>/dev/null
sleep 1

step "start instance A (server UI in scratch tmux)"
$DRV -L $DRVSOCK new-session -d -s drv -x 200 -y 50 "$BIN --port ${APORT} --public 127.0.0.1:${APORT}"
sleep 4
$DRV -L $DRVSOCK capture-pane -p -t drv | tail -4

step "start instance B (client node daemon)"
nohup "$BIN" --port ${BPORT} --connect 127.0.0.1:${APORT} --node-id 127.0.0.1#${BPORT} __remote-daemon >/tmp/wa-e2e-b.log 2>&1 &
echo "B pid=$!"
sleep 6

step "find published remote target for port ${BPORT}"
TARGET=$(grep remote_runtime_owner_upsert $LOG | grep "127.0.0.1#${BPORT}" | tail -1 | sed -n 's/.*target=\([^ ]*\).*/\1/p')
echo "target=[$TARGET]"
if [ -z "$TARGET" ]; then
  echo "FAILED: no remote target published; recent upsert lines:"; grep remote_runtime_owner_upsert $LOG | tail -5
  exit 1
fi

step "locate instance B tmux socket + mirrored pane (precise, via B process args)"
BSOCK=$(ps aux | grep "waitagent" | grep -o "socket-name wa-[0-9a-f]*" | awk '{print $2}' | sort -u | while read -r s; do
  if ps aux | grep -- "--port ${BPORT}" | grep -q -- "$s"; then echo "$s"; fi
done | head -1)
echo "B tmux socket=$BSOCK"
$DRV -L "$BSOCK" list-panes -a -F '#{pane_id} #{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height} pipe=#{pane_pipe}' 2>/dev/null

step "attach from instance A to remote target"
$DRV -L $DRVSOCK new-window -t drv -n attach "$BIN --port ${APORT} --public 127.0.0.1:${APORT} attach '$TARGET'"
sleep 8
$DRV -L $DRVSOCK capture-pane -p -t drv:attach | tail -12

step "remote-main-slot spawned?"
ps aux | grep "__remote-main-slot" | grep -- "--port ${APORT}" | grep -v grep

step "check truthful resize reporting (expect applied=47x22 != requested)"
grep "resize ack geometry mismatch" $LOG | tail -3

step "input gate check: type into the mirror, remote shell must NOT receive it"
BPANE=$($DRV -L "$BSOCK" list-panes -a -F '#{pane_id} #{pane_pipe}' 2>/dev/null | awk '$2=="1"{print $1}' | head -1)
echo "B mirrored pane=$BPANE"
if [ -n "$BPANE" ]; then
  BEFORE=$($DRV -L "$BSOCK" capture-pane -p -t "$BPANE" 2>/dev/null | md5sum | awk '{print $1}')
  $DRV -L $DRVSOCK send-keys -t drv:attach 'l' 's' Enter
  sleep 2
  AFTER=$($DRV -L "$BSOCK" capture-pane -p -t "$BPANE" 2>/dev/null | md5sum | awk '{print $1}')
  echo "B pane md5 before=$BEFORE after=$AFTER"
  if [ "$BEFORE" == "$AFTER" ]; then
    echo "GATE OK: remote shell did not receive input while geometry mismatched"
  else
    echo "GATE FAIL: remote shell received input despite mismatch"
  fi
else
  echo "SKIP: no piped pane found on B"
fi

step "done; instances left running for manual inspection"
echo "cleanup with: pkill -f 'waitagent --port ${APORT}'; pkill -f 'waitagent --port ${BPORT}'; $DRV -L $DRVSOCK kill-server"
