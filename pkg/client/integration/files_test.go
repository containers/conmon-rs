package client_test

import (
	"fmt"
	"os"
	"path/filepath"
)

const (
	// busyboxSource is a copy in our CRI-O bucket of:
	// https://busybox.net/downloads/binaries/1.31.0-i686-uclibc/busybox
	// It only runs on x86, set BUSYBOX_BINARY to a local static busybox on
	// other architectures.
	busyboxSource = "https://storage.googleapis.com/cri-o/test-binaries/busybox"

	// busyboxSHA256 is the checksum of busyboxSource.
	busyboxSHA256 = "d922ebe9067d8ed1b2b9cf326776de40a9b23d4518d674e28a3c14181549d28b"
)

// busyboxDestDir caches the downloaded binary per user, a fixed world
// writable path could be replaced by anyone.
var busyboxDestDir = filepath.Join(os.TempDir(), fmt.Sprintf("conmon-test-images-%d", os.Getuid()))
