#!/usr/bin/env bash
# Create a fourth (or fifth) FDB node as a new guest VM, provisioned exactly the
# way .build-bin/gfs_provision.sh created gfs1/gfs2/gfs-client: a THIN clone of
# the shared Ubuntu 24.04 base image plus a NoCloud seed ISO, defined by
# virt-install's own XML.  Privileged steps go through sudo, so run this as the
# normal user (the ssh key and ~/.ssh/config that get used must be yours).
#
# Why a fourth box at all: .bneck/fdb3vm_fault.sh measured that a `triple`
# cluster on exactly three machines cannot STAY available when one of them dies.
# The log set is three logs and FDB will not put two of them on one machine, so
# after a loss the cluster parks in `recruiting_transaction_servers` ("Unable to
# locate the data distributor worker", "Could not communicate with all of the
# coordination servers") and nothing is writable until the dead machine comes
# back.  With a fourth zone the survivors can re-recruit both the log set and
# three storage replicas, which is the availability property the failure test is
# really about.
#
# Two mistakes this script replaces, recorded so nobody repeats them:
#   - `cp` of an OS qcow2 is NOT a self-contained copy here: gfs*.qcow2 are
#     overlays whose backing file is /data/ifuse/vm/hanjun-vm-base.qcow2.  A thin
#     clone keeps that dependency openly; a copied overlay pretends to be a full
#     image while still reading the shared base (which also means the qcow2
#     backups in /data/grovefs/vmbak are only rollback-capable together with the
#     base image).
#   - hand-editing `virsh dumpxml` and re-defining skips cloud-init, so the clone
#     keeps the source's netplan (which matches on the OLD macaddress) and never
#     gets a lease.  Provisioning through the seed is what makes the identity
#     fresh — hostname, machine-id, ssh host keys and network config alike.
#
# usage: bash .bneck/vm_fdbnode_provision.sh [name]    # default gfs-fdb4
#        bash .bneck/vm_fdbnode_provision.sh gfs-fdb5   # a fifth node, same recipe
set -uo pipefail

URI="qemu:///system"
POOL=/data/ifuse/vm
BASE="$POOL/hanjun-vm-base.qcow2"
PUBKEY="${HOME}/.ssh/id_ed25519.pub"
NAME="${1:-gfs-fdb4}"
MEM_MIB=${FDBNODE_MEM_MIB:-8192}
VCPUS=${FDBNODE_VCPUS:-2}
DATA_GB=${FDBNODE_DATA_GB:-30}

ROOT="$POOL/${NAME}.qcow2"
D1="$POOL/${NAME}-data1.qcow2"
SEED="$POOL/${NAME}-seed.iso"
SDIR="$POOL/${NAME}-seed"

[ -f "$BASE" ] || { echo "base image $BASE missing"; exit 1; }
[ -f "$PUBKEY" ] || { echo "no ssh public key at $PUBKEY"; exit 1; }

echo "[$NAME] thin root clone from base (nothing is copied; the base stays shared) ..."
sudo qemu-img create -f qcow2 -b "$BASE" -F qcow2 "$ROOT" >/dev/null
echo "[$NAME] one ${DATA_GB}G data disk for the fdbserver's data+log dirs ..."
sudo qemu-img create -f qcow2 "$D1" ${DATA_GB}G >/dev/null
sudo chown libvirt-qemu:kvm "$ROOT" "$D1"

echo "[$NAME] cloud-init seed ..."
rm -rf "$SDIR"; mkdir -p "$SDIR"
sudo rm -f "$SEED"     # a leftover seed from a previous attempt is root-owned and would block cloud-localds
cat > "$SDIR/meta-data" <<EOF
instance-id: ${NAME}-001
local-hostname: ${NAME}
EOF

{
cat <<EOF
#cloud-config
hostname: ${NAME}
manage_etc_hosts: true
timezone: Asia/Shanghai

users:
  - name: hanjun
    plain_text_passwd: "hanjun"
    lock_passwd: false
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    groups: [sudo, adm]
    homedir: /home/hanjun
    primary_group: hanjun
    ssh_authorized_keys:
      - $(cat "$PUBKEY")

chpasswd:
  expire: false
ssh_pwauth: true
package_upgrade: false

write_files:
  - path: /etc/ssh/sshd_config.d/60-pw.conf
    permissions: "0644"
    content: |
      PasswordAuthentication yes
      PermitRootLogin no
  # The base image's own 50-cloud-init.yaml matches on the MAC the base had when
  # it was built, and a NoCloud seed without a network-config file does not
  # re-render it.  That is why a thin clone boots with link-local IPv6 and mDNS
  # probes but never sends a DHCP discover: nothing in netplan asks for dhcp4 on
  # the name/mac this guest actually has.  A wildcard match fixes it for any MAC.
  - path: /etc/netplan/90-fdbnode.yaml
    permissions: "0600"
    content: |
      network:
        version: 2
        ethernets:
          fdbnode-en:
            match:
              name: "en*"
            dhcp4: true
            dhcp6: false
            optional: false
  - path: /home/hanjun/.ssh/authorized_keys
    permissions: "0600"
    append: true
    content: |
      $(cat "$PUBKEY")
  - path: /usr/local/sbin/fdb-disksetup.sh
    permissions: "0755"
    content: |
      #!/bin/bash
      # the single data disk becomes /data/fdb, where the fdbserver puts both
      # its data and its log directory
      set -x
      mkdir -p /data/fdb
      dev=\$(lsblk -dno NAME,SIZE | awk '\$2 ~ /${DATA_GB}G/ {print \$1; exit}')
      [ -b "/dev/\$dev" ] || { echo "no ${DATA_GB}G disk found"; exit 0; }
      blkid "/dev/\$dev" >/dev/null 2>&1 || mkfs.ext4 -q -F "/dev/\$dev"
      UUID=\$(blkid -s UUID -o value "/dev/\$dev")
      grep -q " /data/fdb " /etc/fstab || echo "UUID=\$UUID /data/fdb ext4 defaults,nofail 0 2" >> /etc/fstab
      mount /data/fdb || true
runcmd:
  - systemctl enable --now ssh
  - rm -f /etc/ssh/ssh_host_*_key; dpkg-reconfigure openssh-server; systemctl restart ssh
  - echo 'hanjun:hanjun' | chpasswd
  - chown -R hanjun:hanjun /home/hanjun/.ssh
  - rm -f /etc/netplan/50-cloud-init.yaml; netplan apply; systemctl restart systemd-networkd
  - /usr/local/sbin/fdb-disksetup.sh
final_message: "${NAME} ready after \$UPTIME seconds"
EOF
} > "$SDIR/user-data"

cloud-localds "$SEED" "$SDIR/user-data" "$SDIR/meta-data" >/dev/null
chmod 644 "$SEED"      # qemu runs as libvirt-qemu and only needs to read it

echo "[$NAME] virt-install --print-xml, then define/start through libvirt ..."
if ! sudo virsh -c "$URI" dominfo "$NAME" >/dev/null 2>&1; then
  # No on_reboot surgery here on purpose: the gfs guests carry an explicit
  # <on_reboot>destroy</on_reboot> (a guest-side `reboot` powers the box off),
  # which would hang a restart-based recovery test.  virt-install's own XML omits
  # the element, so libvirt's default applies and a reboot really reboots.
  sudo virt-install --connect "$URI" --name "$NAME" --memory $MEM_MIB --vcpus $VCPUS --cpu host \
      --import --disk "path=$ROOT,bus=virtio" --disk "path=$D1,bus=virtio" \
      --cdrom "$SEED" --network network=default,model=virtio \
      --graphics none --noautoconsole --print-xml 2>/dev/null \
      | sed -n '1,/<\/domain>/p' > /tmp/$NAME.xml
  grep -q "<domain" /tmp/$NAME.xml || { echo "[$NAME] virt-install printed no xml"; exit 1; }
  [ "$(grep -c '<domain' /tmp/$NAME.xml)" = 1 ] || { echo "[$NAME] /tmp/$NAME.xml is not a single domain document"; exit 1; }
  sudo virsh -c "$URI" define /tmp/$NAME.xml && sudo rm -f /tmp/$NAME.xml
fi
sudo virsh -c "$URI" start "$NAME"

MAC=$(sudo virsh -c "$URI" domiflist "$NAME" | awk '/52:54:00/{print $NF; exit}')
[ -n "$MAC" ] || { echo "[$NAME] could not read back a MAC from the domain"; exit 1; }
IP=""
echo "[$NAME] waiting for a DHCP lease on $MAC ..."
for _ in $(seq 1 60); do
  # `net-dhcp-leases` prints the expiry as a DATE and a TIME field, so the mac is
  # $3, not $2 — positional parsing here silently matched nothing and the loop
  # "timed out" on a guest that already had a lease.
  IP=$(sudo virsh -c "$URI" net-dhcp-leases default 2>/dev/null | grep -F "$MAC" | \
       awk '{for(i=1;i<=NF;i++) if ($i ~ /^192\.168\./) {split($i,x,"/"); print x[1]; exit}}')
  [ -n "$IP" ] && break
  sleep 4
done
[ -n "$IP" ] || { echo "[$NAME] no lease for $MAC; inspect: sudo virsh -c $URI net-dhcp-leases default, sudo virsh console $NAME"; exit 1; }
echo "[$NAME] mac=$MAC ip=$IP"

for _ in $(seq 1 30); do
  ssh -o ConnectTimeout=6 -o StrictHostKeyChecking=accept-new hanjun@$IP 'true' 2>/dev/null && break
  sleep 5
done
ssh -o StrictHostKeyChecking=accept-new hanjun@$IP 'cloud-init status --wait >/dev/null 2>&1; hostname; df -h /data/fdb 2>/dev/null | tail -1; nproc; free -g | sed -n 2p' 2>&1 | grep -v setlocale

mkdir -p /data/grovefs/vmbak
printf 'NAME=%s\nMAC=%s\nIP=%s\n' "$NAME" "$MAC" "$IP" > /data/grovefs/vmbak/$NAME.ip
if ! grep -q "^Host $NAME" ~/.ssh/config 2>/dev/null; then
  printf '\nHost %s\n  HostName %s\n  User hanjun\n' "$NAME" "$IP" >> ~/.ssh/config
  echo "[$NAME] added to ~/.ssh/config"
fi
echo "PROVISIONED name=$NAME ip=$IP mac=$MAC"
