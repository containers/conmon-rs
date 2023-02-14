#!/usr/bin/env bash

# Packit's default fix-spec-file often doesn't fetch version string correctly.
# This script handles any custom processing of the spec file generated in the
# `post-upstream-clone` action and gets used by the fix-spec-file action
# in .packit.yaml.

set -eo pipefail

# Get Version from HEAD
HEAD_VERSION=$(grep '^version' conmon-rs/common/Cargo.toml | cut -d\" -f2 | sed -e 's/-/~/')

# Generate source tarball from HEAD
git archive --prefix=conmon-rs-$HEAD_VERSION/ -o conmon-rs-$HEAD_VERSION.tar.gz HEAD

# RPM Spec modifications

# Update Version in spec with Version from Cargo.toml
sed -i "s/^Version:.*/Version: $HEAD_VERSION/" conmon-rs.spec

# Update Release in spec with Packit's release envvar
sed -i "s/^Release: %autorelease/Release: $PACKIT_RPMSPEC_RELEASE%{?dist}/" conmon-rs.spec

# Update Source tarball name in spec
sed -i "s/^Source:.*.tar.gz/Source: %{name}-$HEAD_VERSION.tar.gz/" conmon-rs.spec

# Update setup macro to use the correct build dir
sed -i "s/^%setup.*/%autosetup -Sgit -n %{name}-$HEAD_VERSION/" conmon-rs.spec
