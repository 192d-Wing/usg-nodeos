#!/bin/sh
set -eu

target_dir="${TARGET_DIR:?TARGET_DIR is required}"

mkdir -p "$target_dir/etc/nodeos/pki"
mkdir -p "$target_dir/usr/bin"

if [ -x "${NODEOS_INITD:-}" ]; then
  install -m 0755 "$NODEOS_INITD" "$target_dir/usr/bin/initd"
  ln -sf /usr/bin/initd "$target_dir/init"
fi

if [ -x "${NODEOS_NODED:-}" ]; then
  install -m 0755 "$NODEOS_NODED" "$target_dir/usr/bin/noded"
fi

for forbidden in \
  /bin/sh \
  /bin/bash \
  /usr/bin/ssh \
  /usr/sbin/sshd \
  /usr/bin/apt \
  /usr/bin/dnf \
  /usr/bin/yum \
  /usr/bin/apk
do
  if [ -e "$target_dir$forbidden" ]; then
    echo "forbidden production image path exists: $forbidden" >&2
    exit 1
  fi
done

