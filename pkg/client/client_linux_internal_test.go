//go:build linux

package client

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/sirupsen/logrus"
	"golang.org/x/sys/unix"
)

func TestPidfdExitedWhileSignaled(t *testing.T) {
	t.Parallel()

	// The polling thread keeps the only P, the signals would only come
	// between the polls.
	if runtime.GOMAXPROCS(0) < 2 {
		t.Skip("needs GOMAXPROCS of at least 2")
	}

	cmd := startTestServer(t, false)

	pidfd, _ := openPidfd(logrus.New(), uint32(cmd.Process.Pid))
	if pidfd == nil {
		t.Skip("pidfds not supported")
	}

	t.Cleanup(func() { pidfd.Close() })

	// Signals at the thread which polls interrupt the poll, like the
	// preemption of the Go runtime, which uses SIGURG as well.
	runtime.LockOSThread()

	defer runtime.UnlockOSThread()

	pid, tid := os.Getpid(), unix.Gettid()
	stop := make(chan struct{})
	signaled := make(chan error, 1)

	go func() {
		for {
			select {
			case <-stop:
				signaled <- nil

				return
			default:
			}

			if err := unix.Tgkill(pid, tid, unix.SIGURG); err != nil {
				signaled <- err

				return
			}
		}
	}()

	var (
		exited bool
		err    error
	)

	for deadline := time.Now().Add(200 * time.Millisecond); !exited && err == nil && time.Now().Before(deadline); {
		exited, err = pidfdExited(pidfd)
	}

	close(stop)

	if signalErr := <-signaled; signalErr != nil {
		t.Fatalf("signal: %v", signalErr)
	}

	if exited || err != nil {
		t.Fatalf("poll of a running server: exited %v, error %v", exited, err)
	}
}

func TestDialLongSocketWithoutReadPermission(t *testing.T) {
	t.Parallel()

	// Root ignores the missing permission.
	if os.Geteuid() == 0 {
		t.Skip("root has the read permission anyway")
	}

	// Only the search permission, like a run directory with mode 0711 of
	// another user. The path is too long to listen on directly.
	dir := filepath.Join(t.TempDir(), strings.Repeat("d", 120))
	if err := os.Mkdir(dir, 0o700); err != nil {
		t.Fatalf("mkdir: %v", err)
	}

	path := filepath.Join(dir, socketName)
	listenLongSocket(t, path)

	if err := os.Chmod(dir, 0o300); err != nil {
		t.Fatalf("chmod: %v", err)
	}

	// So that the temporary directory can be removed.
	t.Cleanup(func() {
		if err := os.Chmod(dir, 0o700); err != nil {
			t.Errorf("chmod: %v", err)
		}
	})

	conn, err := dialLongSocket("unix", path)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}

	conn.Close()
}

// listenLongSocket listens on a socket whose path is too long for a socket
// address, through the directory of the process like dialLongSocket, until
// the test ends. Closing the listener must not unlink the socket through the
// /proc path, whose descriptor may belong to another test by then, the
// temporary directory gets removed with the test.
func listenLongSocket(t *testing.T, path string) {
	t.Helper()

	dir, err := os.Open(filepath.Dir(path))
	if err != nil {
		t.Fatalf("open directory: %v", err)
	}
	defer dir.Close()

	listener, err := net.ListenUnix("unix", &net.UnixAddr{
		Name: fmt.Sprintf("/proc/self/fd/%d/%s", dir.Fd(), filepath.Base(path)),
		Net:  "unix",
	})
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	listener.SetUnlinkOnClose(false)
	t.Cleanup(func() { listener.Close() })
}
