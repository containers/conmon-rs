package client_test

import (
	"fmt"
	"os"
	"path/filepath"
)

const (
	// copied to our CRI-O bucket from:
	// https://busybox.net/downloads/binaries/1.31.0-i686-uclibc/busybox
	busyboxSource = "https://storage.googleapis.com/cri-o/test-binaries/busybox"
)

// busyboxDestDir caches the downloaded binary per user, a fixed world
// writable path could be replaced by anyone.
var busyboxDestDir = filepath.Join(os.TempDir(), fmt.Sprintf("conmon-test-images-%d", os.Getuid()))
