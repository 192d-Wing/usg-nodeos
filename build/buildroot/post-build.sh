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

# Substitute deployment-specific lab values from .env (sourced by
# build-buildroot-image.sh) into the image config, replacing the documentation
# placeholders shipped in the committed overlay. Unset values keep the
# placeholder. The token/host/IP never live in the committed tree, only in .env.
noded_cfg="$target_dir/etc/nodeos/noded.yaml"
hosts_file="$target_dir/etc/hosts"
subst() { # placeholder value file...
  placeholder="$1"; value="$2"; shift 2
  [ -n "$value" ] || return 0
  for f in "$@"; do
    [ -f "$f" ] && sed -i "s|$placeholder|$value|g" "$f"
  done
}
subst "est.example.com"             "${NODEOS_EST_SERVER:-}"      "$noded_cfg" "$hosts_file"
subst "2001:db8::61"                "${NODEOS_EST_SERVER_IP:-}"   "$hosts_file" "$noded_cfg"
subst "qemu-node-001.example.com"   "${NODEOS_NODE_ID:-}"         "$noded_cfg"
subst "replace-with-bootstrap-token" "${NODEOS_BOOTSTRAP_TOKEN:-}" "$noded_cfg"

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

