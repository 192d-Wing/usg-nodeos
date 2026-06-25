################################################################################
#
# kubelet
#
# The Kubernetes node agent, fetched as the official static linux/amd64 release
# binary (a single executable, not an archive — so EXTRACT just copies it).
#
################################################################################

KUBELET_VERSION = 1.36.2
KUBELET_SITE = https://dl.k8s.io/release/v$(KUBELET_VERSION)/bin/linux/amd64
KUBELET_SOURCE = kubelet
KUBELET_LICENSE = Apache-2.0

define KUBELET_EXTRACT_CMDS
	cp $(KUBELET_DL_DIR)/$(KUBELET_SOURCE) $(@D)/kubelet
endef

define KUBELET_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/kubelet $(TARGET_DIR)/usr/bin/kubelet
endef

$(eval $(generic-package))
