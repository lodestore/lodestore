#!/bin/sh
set -eu

ssh-keygen -A >/dev/null

groupadd lode
for user in srv agent wide nolink; do
  useradd -M -N -g lode -s /usr/sbin/nologin -p '*' "$user"
done

mkdir -p /etc/ssh/authorized
for user in srv agent wide nolink; do
  cp "/keys/$user.pub" "/etc/ssh/authorized/$user"
done
cat /keys/agent_ecdsa.pub >> /etc/ssh/authorized/agent
chmod 0755 /etc/ssh/authorized
chmod 0644 /etc/ssh/authorized/*

mkdir -p /srv/repo/dev /srv/repo/data/tmp /srv/repo/data/packs
chown root:root /srv/repo /srv/repo/dev
chmod 0755 /srv/repo /srv/repo/dev
chown srv:lode /srv/repo/data /srv/repo/data/tmp /srv/repo/data/packs
chmod 2775 /srv/repo/data /srv/repo/data/tmp /srv/repo/data/packs

touch /var/log/sshd.log
socat -u UNIX-RECV:/srv/repo/dev/log,mode=666 OPEN:/var/log/sshd.log,append &

exec /usr/sbin/sshd -D -E /var/log/sshd.log
