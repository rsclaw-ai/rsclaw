#!/usr/bin/env bash
# Cleanup script for GPU worker nodes (Ubuntu 24.04)
# Frees disk space and upgrades NVIDIA driver for RTX 5070
# Usage: sudo bash cleanup-worker.sh
# Target: system ~6GB, 87GB+ free on 98GB disk

set -euo pipefail

echo "=== GPU Worker Node Cleanup ==="
echo ""
echo "Before:"
df -h / | tail -1
echo ""

# --- Remove unnecessary packages ---
echo "[1/8] Removing Java, Rust, LLVM, Docker, Snap, Desktop ..."
apt remove -y --purge \
  openjdk-* default-jdk default-jre \
  rust-1.80-* libstd-rust-* rustc rust-all \
  libllvm17t64 libllvm18 \
  docker.io containerd \
  snapd \
  ubuntu-desktop gnome-* gdm3 nautilus gedit totem eog evince \
  yaru-* ibus-* fonts-noto-cjk \
  2>/dev/null || true
apt autoremove -y

# --- Remove snap/docker leftovers ---
echo "[2/8] Removing snap/docker leftovers ..."
rm -rf /snap /var/snap /var/lib/snapd /var/lib/docker /var/lib/containerd 2>/dev/null || true

# --- Remove CUDA toolkit (PyTorch bundles its own) ---
echo "[3/8] Removing CUDA toolkit ..."
rm -rf /usr/local/cuda-* 2>/dev/null || true

# --- Remove old kernels (keep current) ---
echo "[4/8] Removing old kernels ..."
CURRENT_KERNEL=$(uname -r)
CURRENT_VER=$(echo "$CURRENT_KERNEL" | sed 's/-generic//')
echo "  Current kernel: $CURRENT_KERNEL"
dpkg -l 'linux-modules-extra-*' 'linux-hwe-*-headers-*' 'linux-headers-*' 'linux-image-*' 2>/dev/null \
  | grep '^ii' \
  | awk '{print $2}' \
  | grep -v "$CURRENT_KERNEL" \
  | grep -v "$CURRENT_VER" \
  | xargs -r apt remove -y --purge 2>/dev/null || true
# Remove non-current kernel source/headers from /usr/src
find /usr/src -maxdepth 1 -type d -name 'linux-*' ! -name "*${CURRENT_VER}*" -exec rm -rf {} + 2>/dev/null || true
apt autoremove -y

# --- Clean logs ---
echo "[5/8] Cleaning logs ..."
journalctl --vacuum-size=50M
rm -rf /var/log/*.gz /var/log/*.1 /var/log/*.old
truncate -s 0 /var/log/syslog /var/log/kern.log /var/log/auth.log 2>/dev/null || true

# --- Clean caches ---
echo "[6/8] Cleaning caches ..."
apt clean
rm -rf /tmp/* 2>/dev/null || true
rm -rf ~/.cache/pip ~/.cache/uv 2>/dev/null || true

# --- Set default to CLI (no desktop) ---
echo "[7/8] Setting default boot to CLI ..."
systemctl set-default multi-user.target 2>/dev/null || true

# --- Upgrade NVIDIA driver ---
echo "[8/8] Upgrading NVIDIA driver to 570 ..."
if dpkg -l 'nvidia-driver-570' 2>/dev/null | grep -q '^ii'; then
  echo "  nvidia-driver-570 already installed, skipping"
else
  apt remove -y --purge 'libnvidia-*-535' 'nvidia-*-535' 2>/dev/null || true
  apt install -y nvidia-driver-570
fi

# --- Summary ---
echo ""
echo "=== Done! ==="
echo "After:"
df -h / | tail -1
echo ""
echo "Next steps:"
echo "  1. sudo reboot"
echo "  2. nvidia-smi"
