#!/bin/sh
set -eu

target_dir="${TARGET_DIR:?TARGET_DIR is required}"

# Workload profile this image was built for (k8s | kvm). Exported by
# build-buildroot-image.sh; defaults to k8s for a standalone buildroot invocation.
profile="${NODEOS_PROFILE:-k8s}"
echo "post-build: NodeOS profile = $profile"

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
# build-buildroot-image.sh) into the image config. Substitution is by config KEY
# (replacing whatever value is present), not by matching the committed
# placeholder, so it is idempotent across incremental builds — buildroot may not
# reset the target overlay file, so a prior build's real value would otherwise
# stick. Unset .env values leave the current line untouched. The token/host/IP
# live only in .env, never in the committed tree.
noded_cfg="$target_dir/etc/nodeos/noded.yaml"
hosts_file="$target_dir/etc/hosts"
# Fail closed if the base overlay did not deliver the node config: noded cannot
# start without it, so an absent file is a broken image, not a deferred boot error.
if [ ! -f "$noded_cfg" ]; then
  echo "missing required node config: $noded_cfg (overlay not applied?)" >&2
  exit 1
fi
# The profile is the single source of truth: overwrite noded.yaml's value with the
# build profile so the runtime workload always matches the image (no separate
# per-overlay copy to drift, and no consistency check to maintain).
sed -i -E "s|^([[:space:]]*profile:).*|\1 ${profile}|" "$noded_cfg"
[ -n "${NODEOS_NODE_ID:-}" ] && \
  sed -i -E "s|^([[:space:]]*nodeId:).*|\1 ${NODEOS_NODE_ID}|" "$noded_cfg"
[ -n "${NODEOS_EST_SERVER:-}" ] && \
  sed -i -E "s|^([[:space:]]*serverUrl:).*|\1 https://${NODEOS_EST_SERVER}|" "$noded_cfg"
[ -n "${NODEOS_BOOTSTRAP_TOKEN:-}" ] && \
  sed -i -E "s|^([[:space:]]*bearerToken:).*|\1 ${NODEOS_BOOTSTRAP_TOKEN}|" "$noded_cfg"
# /etc/hosts: replace the EST server mapping (the single est.* entry) by removing
# any existing one and appending the .env value. Idempotent across rebuilds.
if [ -f "$hosts_file" ] && [ -n "${NODEOS_EST_SERVER:-}" ] && [ -n "${NODEOS_EST_SERVER_IP:-}" ]; then
  sed -i -E "/[[:space:]]est\.[^[:space:]]+[[:space:]]*$/d" "$hosts_file"
  printf '%s\t%s\n' "${NODEOS_EST_SERVER_IP}" "${NODEOS_EST_SERVER}" >> "$hosts_file"
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

# Profile isolation: a profile's image must not carry another profile's workload
# binaries. Buildroot's target dir is incremental and does not remove de-selected
# packages, so building two profiles into one output tree leaks (e.g.) the k8s
# runtime into a kvm image. Per-profile output dirs prevent this; this guard
# makes a dirty tree fail loudly instead of shipping a bloated/incorrect image.
if [ "$profile" != "k8s" ]; then
  for k8s_bin in /usr/bin/containerd /usr/bin/kubelet /usr/bin/crictl /usr/bin/ctr; do
    if [ -e "$target_dir$k8s_bin" ]; then
      echo "profile leak: '$profile' image contains k8s binary $k8s_bin" >&2
      echo "       (build each profile in its own buildroot output tree)" >&2
      exit 1
    fi
  done
fi

