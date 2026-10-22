//go:build linux

package client

import (
	"os"
	"runtime"
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
