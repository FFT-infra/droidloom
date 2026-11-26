%define _build_id_links none
%global debug_package %{nil}
%global _enable_debug_package 0
%global __os_install_post %{nil}
%define _binary_payload w1.zstdio
%{!?stage_dir:%global stage_dir %{_topdir}/../stage-image}
%{!?pkg_version:%global pkg_version 0.1.0}
%{!?pkg_release:%global pkg_release 49}

Name:           droidloom-image
Version:        %{pkg_version}
Release:        %{pkg_release}%{?dist}
Summary:        Android system and Droidloom Android components
License:        GPL-3.0-or-later AND LicenseRef-Android AND LicenseRef-Mesa
URL:            https://github.com/denialwm/droidloom
ExclusiveArch:  x86_64 aarch64
AutoReqProv:    no

%description
Immutable Android base partitions and Droidloom Android userspace overrides.

%install
mkdir -p "%{buildroot}"
cp -al "%{stage_dir}/usr" "%{buildroot}/usr"

%files
%defattr(-,root,root,-)
%dir /usr/lib/droidloom
/usr/lib/droidloom/runtime
/usr/lib/droidloom/storage
/usr/lib/droidloom/images
/usr/lib/droidloom/current
/usr/share/licenses/droidloom-image
/usr/share/licenses/droidloom-base
