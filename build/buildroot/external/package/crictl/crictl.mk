################################################################################
#
# crictl
#
# CRI CLI client (cri-tools), official static linux/amd64 release tarball
# (contains a single `crictl` binary).
#
################################################################################

CRICTL_VERSION = 1.36.0
CRICTL_SITE = https://github.com/kubernetes-sigs/cri-tools/releases/download/v$(CRICTL_VERSION)
CRICTL_SOURCE = crictl-v$(CRICTL_VERSION)-linux-amd64.tar.gz
CRICTL_LICENSE = Apache-2.0
# The release tarball holds `crictl` at the archive root (no leading directory),
# so disable buildroot's default --strip-components=1 which would strip it away.
CRICTL_STRIP_COMPONENTS = 0

define CRICTL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/crictl $(TARGET_DIR)/usr/bin/crictl
endef

$(eval $(generic-package))
