################################################################################
#
# containerd-bin
#
# containerd, installed from the official static linux/amd64 release archive
# (bundles containerd, containerd-shim-runc-v2, ctr). Pinned independently of
# Buildroot's in-tree containerd so the k8s profile tracks the desired 2.3.x.
#
################################################################################

CONTAINERD_BIN_VERSION = 2.3.2
CONTAINERD_BIN_SITE = https://github.com/containerd/containerd/releases/download/v$(CONTAINERD_BIN_VERSION)
CONTAINERD_BIN_SOURCE = containerd-static-$(CONTAINERD_BIN_VERSION)-linux-amd64.tar.gz
CONTAINERD_BIN_LICENSE = Apache-2.0

# Archive lays the binaries under bin/ ; buildroot's default --strip-components=1
# drops that prefix, leaving them at $(@D).
define CONTAINERD_BIN_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/containerd $(TARGET_DIR)/usr/bin/containerd
	$(INSTALL) -D -m 0755 $(@D)/containerd-shim-runc-v2 $(TARGET_DIR)/usr/bin/containerd-shim-runc-v2
	$(INSTALL) -D -m 0755 $(@D)/ctr $(TARGET_DIR)/usr/bin/ctr
endef

$(eval $(generic-package))
