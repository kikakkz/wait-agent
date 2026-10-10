#!/bin/sh
# Entrypoint for the install-source e2e sshd container (issue #168).
# Generates host keys, installs the orchestrator-provided authorized_keys
# (bind-mounted at /etc/e2e/authorized_keys), and runs sshd in the foreground.
set -e

mkdir -p /run/sshd /root/.ssh
ssh-keygen -A >/dev/null

if [ ! -f /etc/e2e/authorized_keys ]; then
    echo "e2e-sshd-entrypoint: /etc/e2e/authorized_keys is missing" >&2
    exit 1
fi
cp /etc/e2e/authorized_keys /root/.ssh/authorized_keys
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys

exec /usr/sbin/sshd -D -e
