%define _build_id_links none
%global debug_package %{nil}
%global _enable_debug_package 0
%global __os_install_post %{nil}
%define _binary_payload w3.zstdio
%{!?stage_dir:%global stage_dir %{_topdir}/../stage-runtime}
%{!?pkg_version:%global pkg_version 0.1.0}
%{!?pkg_release:%global pkg_release 49}

Name:           droidloom-runtime
Version:        %{pkg_version}
Release:        %{pkg_release}%{?dist}
Summary:        Android applications in native Wayland windows
License:        GPL-3.0-or-later
URL:            https://github.com/denialwm/droidloom
ExclusiveArch:  x86_64 aarch64
AutoReqProv:    no
Requires:       droidloom-image = %{version}-%{release}
Requires:       glibc, libgcc, mesa-libgbm, mesa-libEGL, mesa-vulkan-drivers
Requires:       libdrm, libwayland-client, libxkbcommon, pipewire-libs, pipewire
Requires:       systemd, polkit, util-linux, e2fsprogs, iproute, nftables, iptables-nft
Requires:       android-tools, vulkan-loader, tar, zstd, policycoreutils
Recommends:     wireplumber

%description
Droidloom Linux Wayland presenter, privileged cell supervisor, CLI, systemd units, and Fedora SELinux policy.

%install
mkdir -p "%{buildroot}"
cp -al "%{stage_dir}/usr" "%{buildroot}/usr"

%pre
if [ "$1" -ge 2 ] && [ -x /usr/lib/droidloom/droidloom-package-helper ]; then
  /usr/lib/droidloom/droidloom-package-helper before-upgrade
fi

%post
/usr/bin/semodule -i /usr/share/selinux/packages/droidloom_fedora.pp
/usr/bin/systemctl daemon-reload >/dev/null 2>&1 || :
/usr/bin/firewall-cmd --reload >/dev/null 2>&1 || :
/usr/lib/droidloom/droidloom-package-helper after-install

%preun
if [ "$1" -eq 0 ]; then
  /usr/lib/droidloom/droidloom-package-helper before-remove
fi

%postun
/usr/bin/systemctl daemon-reload >/dev/null 2>&1 || :
if [ "$1" -eq 0 ]; then
  /usr/bin/semodule -r droidloom_fedora >/dev/null 2>&1 || :
  /usr/bin/firewall-cmd --reload >/dev/null 2>&1 || :
fi

%files
%defattr(-,root,root,-)
/usr/bin/droidloom-applications
/usr/bin/droidloom-audio
/usr/bin/droidloom-devices
/usr/bin/droidloom-doctor
/usr/bin/droidloom-gpu-server
/usr/bin/droidloom-supervisor
/usr/bin/droidloom-wayland
/usr/bin/droidloomctl
/usr/bin/droidloomd
%dir /usr/lib/droidloom
/usr/lib/droidloom/droidloom-package-helper
%ifarch aarch64
/usr/lib/droidloom/fedora-init.rc
/usr/lib/droidloom/fedora-vendor-build.prop
%endif
/usr/lib/firewalld/policies/droidloom-egress.xml
/usr/lib/firewalld/zones/droidloom.xml
/usr/lib/systemd/system/droidloomd.service
/usr/lib/systemd/user/droidloom-applications.service
/usr/lib/systemd/user/droidloom-audio.service
/usr/lib/systemd/user/droidloom-audio.socket
/usr/lib/systemd/user/droidloom-devices.service
/usr/lib/systemd/user/droidloom-devices.socket
/usr/lib/systemd/user/droidloom-gpu.service
/usr/lib/systemd/user/droidloom-gpu.socket
/usr/lib/systemd/user/droidloom-recovery.service
/usr/lib/systemd/user/droidloom.service
/usr/share/doc/droidloom
/usr/share/droidloom
/usr/share/licenses/droidloom-runtime
/usr/share/polkit-1/actions/org.droidloom.setup.policy
/usr/share/selinux/packages/droidloom_fedora.pp
