# The release profile strips the binary, so there is no debug info to extract.
%global debug_package %{nil}

%global bin_name conmonrs

Name: conmon-rs
# Copr builds keep a higher Epoch to always win over distro packages.
%if %{defined copr_username}
Epoch: 101
%endif
Version: 1.2.1
Release: %autorelease
Summary: OCI container runtime monitor written in Rust
License: Apache-2.0 AND BSD-2-Clause AND MIT AND Zlib
URL: https://github.com/containers/%{name}
Source0: %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
BuildRequires: capnproto
# The rust-version of Cargo.toml, make verify-spec checks it.
BuildRequires: cargo >= 1.89
BuildRequires: make
Provides: %{bin_name} = %{?epoch:%{epoch}:}%{version}-%{release}
ExclusiveArch: %{rust_arches}

%description
conmon-rs is a pod level OCI container runtime monitor. It creates the
containers of a pod, writes their logs and serves attach, exec and port forward
sessions. Container engines control it over a Cap'n Proto RPC interface.

%prep
%autosetup

%build
# The source tarball has no git metadata, so pass in what --version shows. A
# commit wins: the build_commit macro, the BUILD_COMMIT environment variable
# (make rpm) or the one packit puts into the Release of its snapshots
# (".g<sha>"). Not for pull requests (".pr<number>."), packit builds them from
# a local merge commit which exists nowhere else. Copr builds are snapshots as
# well, so the version only becomes the tag for other builds without any
# commit, like the ones of releases.
%{lua:
local release = rpm.expand("%{release}")
local commit = rpm.expand("%{?build_commit}")
if commit == "" then commit = os.getenv("BUILD_COMMIT") or "" end
if commit == "" and not string.match(release, "%.pr%d+%.") then
    commit = string.match(release, "%.g(%x+)") or ""
end
if commit ~= "" then
    print("BUILD_COMMIT=" .. commit .. " ")
elseif rpm.expand("%{?copr_username}") == "" then
    print("BUILD_TAG=v" .. rpm.expand("%{version}") .. " ")
end
}%{__make} release

%install
%{__make} DESTDIR=%{buildroot} PREFIX=%{_prefix} install

%files
%license LICENSE
%doc README.md usage.md
%{_bindir}/%{bin_name}

%changelog
%autochangelog
