# The release profile strips the binary, so there is no debug info to extract.
%global debug_package %{nil}

%global bin_name conmonrs

Name: conmon-rs
# Copr builds keep a higher Epoch to always win over distro packages.
%if %{defined copr_username}
Epoch: 101
%endif
Version: 1.0.2
Release: %autorelease
Summary: OCI container runtime monitor written in Rust
License: Apache-2.0 AND BSD-2-Clause AND BSD-3-Clause AND CC0-1.0 AND ISC AND MIT
URL: https://github.com/containers/%{name}
Source0: %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
BuildRequires: capnproto
BuildRequires: cargo
BuildRequires: make
Provides: %{bin_name} = %{?epoch:%{epoch}:}%{version}-%{release}
ExclusiveArch: %{rust_arches}

%description
conmon-rs is a pod level OCI container runtime monitor. It keeps track of the
containers of a pod and serves their logs, attach and exec sessions over a
Cap'n Proto RPC interface.

%prep
%autosetup

%build
%{__make} release

%install
%{__make} DESTDIR=%{buildroot} PREFIX=%{_prefix} install

%files
%license LICENSE
%doc README.md
%{_bindir}/%{bin_name}

%changelog
%autochangelog
