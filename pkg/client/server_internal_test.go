package client

import (
	"context"
	"errors"
	"io"
	"math"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3/exc"
	"github.com/sirupsen/logrus"
	logrustest "github.com/sirupsen/logrus/hooks/test"
)

func TestNewKeepsServerWhichDoesNotAnswer(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	serveSilentSocket(t, dir)

	timeouts := shortTimeouts()
	start := time.Now()
	_, err := New(&ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: "/does/not/exist",
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
		timeouts:         timeouts,
	})

	if !errors.Is(err, ErrServerNotResponding) {
		t.Fatalf("got error %v, want %v", err, ErrServerNotResponding)
	}

	// The listener runs in this process.
	if pid := "PID " + strconv.Itoa(os.Getpid()); !strings.Contains(err.Error(), pid) {
		t.Fatalf("error %q does not name %s", err, pid)
	}

	if elapsed := time.Since(start); elapsed > timeouts.probe+time.Second {
		t.Fatalf("probe took %s, want about %s", elapsed, timeouts.probe)
	}

	if _, err := os.Stat(filepath.Join(dir, socketName)); err != nil {
		t.Fatalf("socket got removed: %v", err)
	}
}

// serveClosingSocket listens on the socket of the run directory and closes
// every connection right away, like a server which shuts down. With
// stopAfterFirst, it stops listening once it closed the first connection,
// like a server which exits. It returns the number of accepted connections.
func serveClosingSocket(t *testing.T, dir string, stopAfterFirst bool) *atomic.Int32 {
	t.Helper()

	listener, err := (&net.ListenConfig{}).Listen(
		t.Context(),
		"unix",
		filepath.Join(dir, socketName),
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	var accepted atomic.Int32

	done := make(chan struct{})

	go func() {
		defer close(done)

		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}

			accepted.Add(1)
			conn.Close()

			if stopAfterFirst {
				listener.Close()

				return
			}
		}
	}()

	t.Cleanup(func() {
		listener.Close()
		<-done
	})

	return &accepted
}

func TestNewReplacesServerWhichGoesAway(t *testing.T) {
	t.Parallel()

	newClient := func(dir string) error {
		_, err := New(&ConmonServerConfig{
			ServerRunDir:     dir,
			ConmonServerPath: "/does/not/exist",
			Runtime:          "/does/not/exist",
			ClientLogger:     logrus.New(),
			timeouts:         shortTimeouts(),
		})

		return err
	}

	// A server which closes the connection of the probe and stops accepting.
	// Without a server binary, the start fails, which shows that New tried.
	dir := t.TempDir()
	accepted := serveClosingSocket(t, dir, true)

	if err := newClient(dir); err == nil || errors.Is(err, ErrServerNotResponding) ||
		!strings.Contains(err.Error(), "start server") {
		t.Fatalf("got error %v, want a failed server start", err)
	}

	// The probe reached the server, it did not just find no socket.
	if n := accepted.Load(); n != 1 {
		t.Fatalf("server accepted %d connections, want 1", n)
	}

	// One which keeps accepting is not replaced.
	dir = t.TempDir()
	serveClosingSocket(t, dir, false)

	if err := newClient(dir); !errors.Is(err, ErrServerNotResponding) {
		t.Fatalf("got error %v, want %v", err, ErrServerNotResponding)
	}
}

func TestServerGoneOnceItsProcessExited(t *testing.T) {
	t.Parallel()

	cmd := exec.CommandContext(t.Context(), "true")
	if err := cmd.Run(); err != nil {
		t.Fatalf("run: %v", err)
	}

	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}

	// Also if something else still holds its socket open.
	if !c.serverGone(t.Context(), int32(cmd.Process.Pid), true, false) {
		t.Fatal("exited server not gone")
	}

	// A running one which did not close the connection is not gone.
	if c.serverGone(t.Context(), int32(os.Getpid()), true, false) {
		t.Fatal("running server gone")
	}

	if c.serverGone(t.Context(), 0, false, false) {
		t.Fatal("server with unknown PID gone")
	}
}

func TestNewStartsServerOnlyWithoutListener(t *testing.T) {
	t.Parallel()

	newClient := func(dir string) error {
		_, err := New(&ConmonServerConfig{
			ServerRunDir:     dir,
			ConmonServerPath: "/does/not/exist",
			Runtime:          "/does/not/exist",
			ClientLogger:     logrus.New(),
		})

		return err
	}

	// The socket of a server which crashed refuses the connection.
	stale := t.TempDir()

	listener, listenErr := net.ListenUnix(
		"unix",
		&net.UnixAddr{Name: filepath.Join(stale, socketName), Net: "unix"},
	)
	if listenErr != nil {
		t.Fatalf("listen: %v", listenErr)
	}

	listener.SetUnlinkOnClose(false)
	listener.Close()

	// Without a server binary, the start fails, which shows that New tried.
	for _, dir := range []string{stale, t.TempDir()} {
		err := newClient(dir)
		if err == nil || errors.Is(err, ErrServerNotResponding) ||
			!strings.Contains(err.Error(), "start server") {
			t.Fatalf("got error %v, want a failed server start", err)
		}
	}

	if os.Geteuid() == 0 {
		return
	}

	// A socket which cannot be dialed for other reasons is no reason to
	// start another server either.
	locked := t.TempDir()
	if err := os.Chmod(locked, 0); err != nil {
		t.Fatalf("chmod: %v", err)
	}

	t.Cleanup(func() {
		if err := os.Chmod(locked, 0o700); err != nil {
			t.Errorf("chmod: %v", err)
		}
	})

	runDir := filepath.Join(locked, "run")
	cl := &ConmonClient{runDir: runDir, logger: logrus.New()}

	_, err := cl.connectRunningServer(t.Context())
	if !errors.Is(err, os.ErrPermission) || errors.Is(err, ErrServerNotResponding) {
		t.Fatalf("got error %v, want a permission error", err)
	}
}

// chanWriter sends every write to a channel.
type chanWriter chan string

func (w chanWriter) Write(p []byte) (int, error) {
	w <- string(p)

	return len(p), nil
}

// sliceWriter is a writer whose type is not comparable.
type sliceWriter []byte

func (sliceWriter) Write(p []byte) (int, error) {
	return len(p), nil
}

func TestSameWriter(t *testing.T) {
	t.Parallel()

	w := make(chanWriter)

	if !sameWriter(w, w) {
		t.Error("same writer not detected")
	}

	if sameWriter(w, make(chanWriter)) {
		t.Error("different writers detected as the same")
	}

	if sameWriter(sliceWriter{}, sliceWriter{}) {
		t.Error("writers which are not comparable detected as the same")
	}
}

func TestServerOutputFile(t *testing.T) {
	t.Parallel()

	logger := logrus.New()

	file, release, err := serverOutputFile(logger, os.Stdout)
	if err != nil || file != os.Stdout {
		t.Fatalf("a file is not passed as it is: %v, %v", file, err)
	}

	release()

	output := make(chanWriter, 1)

	file, release, err = serverOutputFile(logger, output)
	if err != nil {
		t.Fatalf("create server output file: %v", err)
	}

	// The copy outlives the release of the parent's end, like the server
	// keeps writing after it got launched.
	cmd := exec.CommandContext(context.Background(), "sh", "-c", "sleep 0.1; echo -n hello")
	cmd.Stdout = file

	if err := cmd.Start(); err != nil {
		t.Fatalf("start command: %v", err)
	}

	release()

	if err := cmd.Wait(); err != nil {
		t.Fatalf("wait for command: %v", err)
	}

	select {
	case got := <-output:
		if got != "hello" {
			t.Errorf("got output %q, want %q", got, "hello")
		}
	case <-time.After(10 * time.Second):
		t.Fatal("timed out waiting for the output")
	}
}

func TestProcessExited(t *testing.T) {
	t.Parallel()

	if processExited(uint32(os.Getpid())) {
		t.Error("own process reported as exited")
	}

	cmd := exec.CommandContext(context.Background(), "true")
	if err := cmd.Start(); err != nil {
		t.Fatalf("start command: %v", err)
	}

	pid := uint32(cmd.Process.Pid)

	// Not reaped until the zombie got checked, so the PID cannot get reused
	// before. The zombie does not run anymore either.
	waitFor(t, "zombie not reported as exited", func() bool { return processExited(pid) })

	if err := cmd.Wait(); err != nil {
		t.Fatalf("wait for command: %v", err)
	}

	// A reaped PID could get reused right away, one above the maximum can
	// never exist.
	pidMax, err := os.ReadFile("/proc/sys/kernel/pid_max")
	if err != nil {
		t.Fatalf("read pid_max: %v", err)
	}

	maxPID, err := strconv.ParseUint(strings.TrimSpace(string(pidMax)), 10, 32)
	if err != nil {
		t.Fatalf("parse pid_max: %v", err)
	}

	if !processExited(uint32(maxPID + 1)) {
		t.Error("process which cannot exist not reported as exited")
	}
}

func TestProcessStartTime(t *testing.T) {
	t.Parallel()

	pid := uint32(os.Getpid())

	start := processStartTime(pid)
	if start == 0 {
		t.Skip("/proc not readable")
	}

	if again := processStartTime(pid); again != start {
		t.Fatalf("start time changed from %d to %d", start, again)
	}

	if got := processStartTime(math.MaxUint32); got != 0 {
		t.Fatalf("got start time %d of a process which cannot exist", got)
	}
}

func TestServerExitedWithoutPidfdComparesStartTimes(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, false)
	pid := uint32(cmd.Process.Pid)

	start := processStartTime(pid)
	if start == 0 {
		t.Skip("/proc not readable")
	}

	c := newTestClient(t, t.TempDir(), logrus.New())

	// The process which runs with the PID is the recorded server.
	if c.serverExited(&serverProcess{pid: pid, startTime: start}) {
		t.Fatal("running server reported as exited")
	}

	// It got the PID of a server which exited.
	if !c.serverExited(&serverProcess{pid: pid, startTime: start - 1}) {
		t.Fatal("process which got the PID of the server taken for it")
	}

	// Without a start time, the PID decides.
	if c.serverExited(&serverProcess{pid: pid}) {
		t.Fatal("running server without start time reported as exited")
	}
}

// startTestServer starts a process which stands in for the server, the
// command gets waited for with the test. With ignoreInt, SIGINT does not end
// it.
func startTestServer(t *testing.T, ignoreInt bool) *exec.Cmd {
	t.Helper()

	script := "exec sleep 60"
	if ignoreInt {
		script = "trap '' INT; " + script
	}

	cmd := exec.CommandContext(context.Background(), "sh", "-c", script)
	if err := cmd.Start(); err != nil {
		t.Fatalf("start: %v", err)
	}

	t.Cleanup(func() {
		_ = cmd.Process.Kill() //nolint:errcheck // may have exited already
		_ = cmd.Wait()         //nolint:errcheck // killed on purpose
	})

	// The shell sets up the trap before it execs, wait for that.
	if ignoreInt {
		waitFor(t, "test server did not exec", func() bool {
			comm, err := os.ReadFile("/proc/" + strconv.Itoa(cmd.Process.Pid) + "/comm")

			return err == nil && strings.TrimSpace(string(comm)) == "sleep"
		})
	}

	return cmd
}

func TestShutdown(t *testing.T) {
	t.Parallel()

	for _, withPidfd := range []bool{true, false} {
		t.Run("pidfd "+strconv.FormatBool(withPidfd), func(t *testing.T) {
			t.Parallel()

			newClient := func(pid int) *ConmonClient {
				c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}

				c.setServerPID(uint32(pid))

				if !withPidfd && c.server.pidfd != nil {
					c.server.pidfd.Close()
					c.server.pidfd = nil
				} else if withPidfd && c.server.pidfd == nil {
					t.Skip("pidfds not supported")
				}

				return c
			}

			// The zombie of the server counts as gone.
			cmd := startTestServer(t, false)
			c := newClient(cmd.Process.Pid)

			if err := c.Shutdown(); err != nil {
				t.Fatalf("shutdown: %v", err)
			}

			if err := c.Shutdown(); err != nil {
				t.Fatalf("shutdown again: %v", err)
			}

			// Only a server which gets killed exits.
			cmd = startTestServer(t, true)
			c = newClient(cmd.Process.Pid)

			if err := c.stopServer(
				t.Context(),
				c.server,
				200*time.Millisecond,
				false,
			); !errors.Is(
				err,
				ErrShutdownTimeout,
			) {
				t.Fatalf("got error %v, want %v", err, ErrShutdownTimeout)
			}

			if err := c.stopServer(t.Context(), c.server, 200*time.Millisecond, true); err != nil {
				t.Fatalf("stop with kill: %v", err)
			}

			if !processExited(uint32(cmd.Process.Pid)) {
				t.Fatal("server still running after the kill")
			}

			// The server is gone for good, its pidfd got released.
			if !c.server.done.Load() {
				t.Fatal("server not marked as gone")
			}

			if withPidfd && !errors.Is(c.server.pidfd.Close(), os.ErrClosed) {
				t.Fatal("pidfd still open")
			}
		})
	}
}

func TestShutdownServerWhichExited(t *testing.T) {
	t.Parallel()

	cmd := exec.CommandContext(context.Background(), "true")
	if err := cmd.Start(); err != nil {
		t.Fatalf("start: %v", err)
	}

	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	c.setServerPID(uint32(cmd.Process.Pid))

	if c.server.pidfd == nil {
		t.Skip("pidfds not supported")
	}

	// Reaped, so the PID could belong to another process by now. The pidfd
	// still refers to the exited one.
	if err := cmd.Wait(); err != nil {
		t.Fatalf("wait: %v", err)
	}

	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}
}

func TestSetServerPID(t *testing.T) {
	t.Parallel()

	first := startTestServer(t, false)
	second := startTestServer(t, false)

	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	c.setServerPID(uint32(first.Process.Pid))

	old := c.server.pidfd
	if old == nil {
		t.Skip("pidfds not supported")
	}

	// The server reports another PID than the PID file named.
	c.setServerPID(uint32(second.Process.Pid))

	if !errors.Is(old.Close(), os.ErrClosed) {
		t.Fatal("pidfd of the old PID still open")
	}

	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}

	if !processExited(uint32(second.Process.Pid)) {
		t.Fatal("server of the new PID still running")
	}

	if processExited(uint32(first.Process.Pid)) {
		t.Fatal("process of the old PID got signaled")
	}

	// A server which is gone already never gets signaled through its PID,
	// which may belong to another process by now.
	gone := exec.CommandContext(context.Background(), "true")
	if err := gone.Run(); err != nil {
		t.Fatalf("run: %v", err)
	}

	c = &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	c.setServerPID(uint32(gone.Process.Pid))

	if c.server.pidfd != nil {
		t.Skip("PID got reused")
	}

	if !c.server.done.Load() {
		t.Fatal("exited server not marked as gone")
	}

	if err := c.server.signal(syscall.SIGINT); !errors.Is(err, syscall.ESRCH) {
		t.Fatalf("got error %v, want %v", err, syscall.ESRCH)
	}

	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}
}

// pidFromFile waits for the PID file of a test server binary in dir, the
// one New reads unless another name is given.
func pidFromFile(t *testing.T, dir string, name ...string) int {
	t.Helper()

	var pid int

	file := filepath.Join(dir, pidFileName)
	if len(name) > 0 {
		file = filepath.Join(dir, name[0])
	}

	waitFor(t, "no PID file", func() bool {
		data, err := os.ReadFile(file)
		if err != nil {
			return false
		}

		pid, err = strconv.Atoi(strings.TrimSpace(string(data)))

		return err == nil
	})

	return pid
}

// killOnCleanup kills a process which the test did not start itself.
func killOnCleanup(t *testing.T, pid int) {
	t.Helper()

	t.Cleanup(func() {
		if !processExited(uint32(pid)) {
			_ = syscall.Kill(pid, syscall.SIGKILL) //nolint:errcheck // best effort
		}
	})
}

func TestNewKillsServerWhichDoesNotComeUp(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// A server which ignores SIGINT and never listens. The arguments are
	// --runtime, the runtime, --runtime-dir and the run directory. New
	// removes the PID file once it failed, the test reads a copy.
	binary := writeTestBinary(
		t,
		`(trap '' INT; exec sleep 60) </dev/null >/dev/null 2>&1 & `+
			`printf %s $! >"$4/pidfile"; printf %s $! >"$4/pid"`,
	)

	const ctxTimeout = 500 * time.Millisecond

	ctx, cancel := context.WithTimeout(t.Context(), ctxTimeout)
	defer cancel()

	timeouts := shortTimeouts()
	start := time.Now()

	_, err := NewContext(ctx, &ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: binary,
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
		timeouts:         timeouts,
	})
	if !errors.Is(err, ErrServerNotUp) || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("got error %v, want %v and %v", err, ErrServerNotUp, context.DeadlineExceeded)
	}

	pid := pidFromFile(t, dir, "pid")
	killOnCleanup(t, pid)

	if !processExited(uint32(pid)) {
		t.Fatal("server still running")
	}

	if _, err := os.Stat(filepath.Join(dir, pidFileName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("PID file not removed: %v", err)
	}

	// SIGINT did not end it, SIGKILL did after the cleanup wait, which
	// starts once the context expired.
	if elapsed := time.Since(start); elapsed < ctxTimeout+timeouts.cleanup {
		t.Fatalf("server gone after %s, before the SIGKILL", elapsed)
	}
}

func TestNewTakesPIDFromServer(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	server := startTestServer(t, false)

	// The PID file names another process than the server reports.
	binary := writeTestBinary(
		t,
		`sleep 60 </dev/null >/dev/null 2>&1 & printf %s $! >"$4/pidfile"`,
	)

	type result struct {
		client *ConmonClient
		err    error
	}

	done := make(chan result, 1)

	go func() {
		cl, err := NewContext(t.Context(), &ConmonServerConfig{
			ServerRunDir:     dir,
			ConmonServerPath: binary,
			Runtime:          "/does/not/exist",
			ClientLogger:     logrus.New(),
		})
		done <- result{cl, err}
	}()

	// The server only listens once it got started.
	other := pidFromFile(t, dir)
	killOnCleanup(t, other)

	serveVersionSocket(t, dir, versionConmon{
		pid:      uint32(server.Process.Pid),
		features: []string{FeatureSdNotify},
	})

	res := <-done
	if res.err != nil {
		t.Fatalf("new: %v", res.err)
	}

	cl := res.client

	if cl.PID() != uint32(server.Process.Pid) {
		t.Fatalf("got PID %d, want %d", cl.PID(), server.Process.Pid)
	}

	// The features of the answer got recorded.
	if !cl.SupportsFeature(FeatureSdNotify) || cl.SupportsFeature(FeaturePortForward) {
		t.Fatalf("got features %v, want only %q", cl.serverFeatures(), FeatureSdNotify)
	}

	if _, err := os.Stat(cl.pidFile()); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("PID file not removed: %v", err)
	}

	if err := cl.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}

	if !processExited(uint32(server.Process.Pid)) {
		t.Fatal("server still running")
	}

	if processExited(uint32(other)) {
		t.Fatal("process of the PID file got signaled")
	}
}

func TestNewContextCancelled(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(t.Context())
	cancel()

	for _, running := range []bool{false, true} {
		t.Run("running "+strconv.FormatBool(running), func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			if running {
				//nolint:contextcheck // serves for the whole test, not only the cancelled call
				serveSilentSocket(t, dir)
			}

			// The binary leaves a file behind if it gets started.
			//nolint:contextcheck // writes the binary for the whole test
			binary := writeTestBinary(t, `touch "$4/started"`)

			_, err := NewContext(ctx, &ConmonServerConfig{
				ServerRunDir:     dir,
				ConmonServerPath: binary,
				Runtime:          "/does/not/exist",
				ClientLogger:     logrus.New(),
			})
			if !errors.Is(err, context.Canceled) {
				t.Fatalf("got error %v, want %v", err, context.Canceled)
			}

			// The error of the probe stays visible next to the context one.
			if errors.Is(err, ErrServerNotResponding) != running {
				t.Fatalf("got error %v, want ErrServerNotResponding: %v", err, running)
			}

			if _, err := os.Stat(filepath.Join(dir, "started")); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("server got started: %v", err)
			}
		})
	}
}

func TestShutdownWithoutPID(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())
	t.Cleanup(func() { c.Close() })

	stdin, _ := io.Pipe()

	_, session, err := c.startAttachSession(t.Context(), &AttachConfig{
		Streams: AttachStreams{Stdin: &In{stdin}},
	})
	if err != nil {
		t.Fatalf("start attach session: %v", err)
	}

	// A zero PID would signal the process group of the caller. Nothing gets
	// stopped then, the attach sessions keep their stdin.
	for name, stop := range map[string]func() error{
		"Shutdown":    c.Shutdown,
		"Kill":        c.Kill,
		"KillContext": func() error { return c.KillContext(t.Context()) },
	} {
		if err := stop(); !errors.Is(err, ErrServerPIDUnknown) {
			t.Fatalf("%s: got error %v, want %v", name, err, ErrServerPIDUnknown)
		}
	}

	if session.stdinClosed.Load() {
		t.Fatal("stdin of the attach session got closed")
	}
}

// replacedBySamePID records a server, which exits, and serves a server which
// answers with its PID on the socket of the client, like a replacement which
// got the same PID.
func replacedBySamePID(t *testing.T) (*ConmonClient, *serverProcess) {
	t.Helper()

	cmd := startTestServer(t, false)
	pid := uint32(cmd.Process.Pid)

	dir := t.TempDir()
	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	c.setServerPID(pid)
	recorded := c.currentServer()

	if err := cmd.Process.Kill(); err != nil {
		t.Fatalf("kill: %v", err)
	}

	_ = cmd.Wait() //nolint:errcheck // killed on purpose

	serveVersionSocket(t, dir, versionConmon{pid: pid})

	return c, recorded
}

func TestReplacementWithTheSamePIDGetsPickedUp(t *testing.T) {
	t.Parallel()

	t.Run("pickUpReplacement", func(t *testing.T) {
		t.Parallel()

		c, recorded := replacedBySamePID(t)
		c.pickUpReplacement(t.Context())

		if c.currentServer() == recorded {
			t.Fatal("kept the recorded server which exited")
		}
	})

	t.Run("refreshServer", func(t *testing.T) {
		t.Parallel()

		c, recorded := replacedBySamePID(t)
		// Only with recorded features, the first call asks for them.
		c.setFeatures(nil)

		if _, err := c.Version(t.Context(), nil); err != nil {
			t.Fatalf("version: %v", err)
		}

		if c.currentServer() == recorded {
			t.Fatal("kept the recorded server which exited")
		}
	})
}

func TestRecordedServerWhichRunsStays(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, false)
	pid := uint32(cmd.Process.Pid)

	dir := t.TempDir()
	serveVersionSocket(t, dir, versionConmon{pid: pid})

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	c.setServerPID(pid)
	c.setFeatures(nil)
	recorded := c.currentServer()

	c.pickUpReplacement(t.Context())

	if _, err := c.Version(t.Context(), nil); err != nil {
		t.Fatalf("version: %v", err)
	}

	if c.currentServer() != recorded {
		t.Fatal("replaced the recorded server which still runs")
	}
}

func TestRefreshServerKeepsTheRecordedOnesWithoutAnswer(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, false)

	dir := t.TempDir()
	// A server which never answers Version.
	serveVersionSocket(t, dir, versionConmon{
		pid:      uint32(os.Getpid()),
		features: []string{"other"},
		release:  make(chan struct{}),
	})

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	c.setServerPID(uint32(cmd.Process.Pid))
	c.setFeatures([]string{"recorded"})
	recorded := c.currentServer()

	conn, rpcClient, dialed, err := c.usableConn(t.Context())
	if err != nil || !dialed {
		t.Fatalf("dial: %v, dialed %v", err, dialed)
	}
	defer rpcClient.Release()

	ctx, cancel := context.WithTimeout(t.Context(), 100*time.Millisecond)
	defer cancel()

	c.refreshServer(ctx, conn, rpcClient)

	if c.currentServer() != recorded {
		t.Fatal("replaced the recorded server without an answer")
	}

	if !c.SupportsFeature("recorded") || c.SupportsFeature("other") {
		t.Fatal("replaced the recorded features without an answer")
	}
}

func TestPickUpReplacementWithoutReplacement(t *testing.T) {
	t.Parallel()

	for _, closed := range []bool{false, true} {
		t.Run("closed "+strconv.FormatBool(closed), func(t *testing.T) {
			t.Parallel()

			cmd := startTestServer(t, false)

			dir := t.TempDir()
			c := newTestClient(t, dir, logrus.New())
			c.setServerPID(uint32(cmd.Process.Pid))
			recorded := c.currentServer()

			if err := cmd.Process.Kill(); err != nil {
				t.Fatalf("kill: %v", err)
			}

			_ = cmd.Wait() //nolint:errcheck // killed on purpose

			if closed {
				// A server answers, but a closed client does not look.
				serveVersionSocket(t, dir, versionConmon{pid: uint32(os.Getpid())})

				if err := c.Close(); err != nil {
					t.Fatalf("close: %v", err)
				}
			} else {
				// Nothing listens on the socket, the dial fails.
				t.Cleanup(func() { c.Close() })
			}

			c.pickUpReplacement(t.Context())

			if c.currentServer() != recorded {
				t.Fatal("replaced the recorded server")
			}
		})
	}
}

func TestShutdownAfterReconnectToAnotherServer(t *testing.T) {
	t.Parallel()

	first := startTestServer(t, false)
	second := startTestServer(t, false)

	dir := t.TempDir()
	stop := serveVersionSocket(t, dir, versionConmon{pid: uint32(first.Process.Pid)})

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	connected, err := c.connectRunningServer(t.Context())
	if err != nil || !connected {
		t.Fatalf("connect: %v, connected %v", err, connected)
	}

	old := c.server
	if old.pidfd == nil {
		t.Skip("pidfds not supported")
	}

	// Another server replaces the one the client is connected to.
	conn := c.conn

	stop()
	waitBroken(t, conn)

	serveVersionSocket(t, dir, versionConmon{pid: uint32(second.Process.Pid)})

	if _, err := c.Version(t.Context(), nil); err != nil {
		t.Fatalf("version after reconnect: %v", err)
	}

	if got := c.PID(); got != uint32(second.Process.Pid) {
		t.Fatalf("got PID %d, want %d", got, second.Process.Pid)
	}

	if !errors.Is(old.pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd of the replaced server still open")
	}

	// Shutdown stops the server the client talks to.
	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}

	if !processExited(uint32(second.Process.Pid)) {
		t.Fatal("server which replaced the first one still running")
	}

	if processExited(uint32(first.Process.Pid)) {
		t.Fatal("replaced server got signaled")
	}
}

func TestSetServerPIDIf(t *testing.T) {
	t.Parallel()

	first := startTestServer(t, false)
	second := startTestServer(t, false)
	third := startTestServer(t, false)

	c := newTestClient(t, t.TempDir(), logrus.New())
	t.Cleanup(func() { c.Close() })

	c.setServerPID(uint32(first.Process.Pid))
	read := c.currentServer()

	// Another call recorded a server in the meantime, which stays.
	c.setServerPID(uint32(second.Process.Pid))

	if c.setServerPIDIf(read, uint32(third.Process.Pid)) {
		t.Fatal("replaced a server which got recorded in the meantime")
	}

	if got := c.PID(); got != uint32(second.Process.Pid) {
		t.Fatalf("got PID %d, want %d", got, second.Process.Pid)
	}

	if !c.setServerPIDIf(c.currentServer(), uint32(third.Process.Pid)) {
		t.Fatal("did not replace the recorded server")
	}

	if got := c.PID(); got != uint32(third.Process.Pid) {
		t.Fatalf("got PID %d, want %d", got, third.Process.Pid)
	}
}

func TestShutdownPicksUpReplacement(t *testing.T) {
	t.Parallel()

	for _, kill := range []bool{false, true} {
		t.Run("kill "+strconv.FormatBool(kill), func(t *testing.T) {
			t.Parallel()

			first := startTestServer(t, false)
			second := startTestServer(t, false)

			dir := t.TempDir()

			c := newTestClient(t, dir, logrus.New())
			c.setServerPID(uint32(first.Process.Pid))

			// The recorded server exited and another one took over the
			// socket, no call connected to it yet.
			if err := first.Process.Kill(); err != nil {
				t.Fatalf("kill: %v", err)
			}

			_ = first.Wait() //nolint:errcheck // killed on purpose

			serveVersionSocket(t, dir, versionConmon{pid: uint32(second.Process.Pid)})

			stop := c.Shutdown
			if kill {
				stop = func() error { return c.KillContext(t.Context()) }
			}

			if err := stop(); err != nil {
				t.Fatalf("stop: %v", err)
			}

			if !processExited(uint32(second.Process.Pid)) {
				t.Fatal("server which replaced the recorded one still running")
			}
		})
	}
}

func TestReplacedServerKeepsPidfdDuringShutdown(t *testing.T) {
	t.Parallel()

	first := startTestServer(t, false)
	second := startTestServer(t, false)

	c := newTestClient(t, t.TempDir(), logrus.New())
	c.setServerPID(uint32(first.Process.Pid))

	if c.server.pidfd == nil {
		t.Skip("pidfds not supported")
	}

	// A shutdown in progress keeps the server it started with.
	server, release, err := c.usePidfd()
	if err != nil {
		t.Fatalf("use pidfd: %v", err)
	}

	c.setServerPID(uint32(second.Process.Pid))

	if err := c.stopServer(t.Context(), server, serverStopTimeout, false); err != nil {
		t.Fatalf("stop server: %v", err)
	}

	release()

	if !processExited(uint32(first.Process.Pid)) {
		t.Fatal("server of the shutdown still running")
	}

	if processExited(uint32(second.Process.Pid)) {
		t.Fatal("server which replaced it got signaled")
	}

	if !errors.Is(server.pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd of the replaced server still open")
	}

	if c.server.pidfd == nil || errors.Is(c.server.pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd of the current server got closed")
	}
}

// writeTestBinary writes a shell script which stands in for the server
// binary.
func writeTestBinary(t *testing.T, script string) string {
	t.Helper()

	path := filepath.Join(t.TempDir(), "conmonrs")

	// Written by another process: a file this process writes can still be
	// open for writing in a child of a parallel test which did not exec
	// yet, running it would fail with ETXTBSY then.
	cmd := exec.CommandContext(t.Context(), "sh", "-c", `cat >"$1" && chmod 700 "$1"`, "sh", path)
	cmd.Stdin = strings.NewReader("#!/bin/sh\n" + script + "\n")

	if output, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("write binary: %v: %s", err, output)
	}

	return path
}

func TestClose(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	server := startTestServer(t, false)

	serveVersionSocket(t, dir, versionConmon{pid: uint32(server.Process.Pid)})

	c := newTestClient(t, dir, logrus.New())
	c.setServerPID(uint32(server.Process.Pid))

	if _, err := c.Version(t.Context(), nil); err != nil {
		t.Fatalf("version: %v", err)
	}

	// An attach session in progress ends.
	sessionCtx, _, err := c.startAttachSession(t.Context(), &AttachConfig{})
	if err != nil {
		t.Fatalf("start attach session: %v", err)
	}

	pidfd := c.server.pidfd

	if err = c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if err = context.Cause(sessionCtx); !errors.Is(err, ErrClientClosed) {
		t.Fatalf("got attach session error %v, want %v", err, ErrClientClosed)
	}

	// One which gets set up after Close fails.
	_, _, err = c.startAttachSession(t.Context(), &AttachConfig{})
	if !errors.Is(err, ErrClientClosed) {
		t.Fatalf("got error %v starting an attach session, want %v", err, ErrClientClosed)
	}

	if len(c.attachSessions) != 0 {
		t.Fatalf("attach sessions stay registered: %d", len(c.attachSessions))
	}

	if c.conn != nil {
		t.Fatal("connection still open")
	}

	if pidfd != nil && !errors.Is(pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd still open")
	}

	if _, err := c.Version(t.Context(), nil); !errors.Is(err, ErrClientClosed) {
		t.Fatalf("got error %v, want %v", err, ErrClientClosed)
	}

	for name, stop := range map[string]func(context.Context) error{
		"ShutdownContext": c.ShutdownContext,
		"KillContext":     c.KillContext,
	} {
		if err := stop(t.Context()); !errors.Is(err, ErrClientClosed) {
			t.Fatalf("%s: got error %v, want %v", name, err, ErrClientClosed)
		}
	}

	// The server keeps running.
	if processExited(uint32(server.Process.Pid)) {
		t.Fatal("server got stopped")
	}

	if err := c.Close(); err != nil {
		t.Fatalf("close again: %v", err)
	}
}

func TestCloseFailsCallInProgress(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	called, release := make(chan struct{}, 1), make(chan struct{})

	t.Cleanup(func() { close(release) })

	serveVersionSocket(t, dir, versionConmon{called: called, release: release})

	c := newTestClient(t, dir, logrus.New())
	versionErr := make(chan error, 1)

	go func() {
		_, err := c.Version(t.Context(), nil)
		versionErr <- err
	}()

	<-called

	if err := c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	select {
	case err := <-versionErr:
		// The call fails with the connection it used.
		var exception *exc.Exception
		if !errors.Is(err, ErrClientClosed) &&
			(!errors.As(err, &exception) || exception.Type != exc.Disconnected) {
			t.Fatalf("got error %v, want %v or a connection error", err, ErrClientClosed)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("call still in progress after Close")
	}
}

func TestShutdownClosesAttachStdin(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, false)

	c := newTestClient(t, t.TempDir(), logrus.New())
	c.setServerPID(uint32(cmd.Process.Pid))

	stdin, stdinWriter := io.Pipe()

	sessionCtx, session, err := c.startAttachSession(
		t.Context(),
		&AttachConfig{Streams: AttachStreams{Stdin: &In{stdin}}},
	)
	if err != nil {
		t.Fatalf("start attach session: %v", err)
	}
	defer c.endAttachSession(session)

	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}

	if _, err := stdinWriter.Write([]byte("x")); !errors.Is(err, io.ErrClosedPipe) {
		t.Fatalf("got error %v writing to the attach stdin, want %v", err, io.ErrClosedPipe)
	}

	// Unlike Close, Shutdown does not end the session, the exit of the
	// server does.
	if err := sessionCtx.Err(); err != nil {
		t.Fatalf("attach session ended with %v", err)
	}
}

func TestKillContext(t *testing.T) {
	t.Parallel()

	// A server which ignores SIGINT outlives the shutdown, the kill ends it.
	cmd := startTestServer(t, true)

	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	c.setServerPID(uint32(cmd.Process.Pid))

	if err := c.stopServer(
		t.Context(),
		c.server,
		200*time.Millisecond,
		false,
	); !errors.Is(
		err,
		ErrShutdownTimeout,
	) {
		t.Fatalf("got error %v, want %v", err, ErrShutdownTimeout)
	}

	if err := c.KillContext(t.Context()); err != nil {
		t.Fatalf("kill: %v", err)
	}

	if !processExited(uint32(cmd.Process.Pid)) {
		t.Fatal("server still running after the kill")
	}

	// A server which is gone already is no error.
	if err := c.Kill(); err != nil {
		t.Fatalf("kill again: %v", err)
	}

	// A zero PID would signal the process group of the caller.
	c = &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	if err := c.KillContext(t.Context()); !errors.Is(err, ErrServerPIDUnknown) {
		t.Fatalf("got error %v, want %v", err, ErrServerPIDUnknown)
	}
}

func TestCloseDuringShutdownKeepsPidfd(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, false)

	c := newTestClient(t, t.TempDir(), logrus.New())
	c.setServerPID(uint32(cmd.Process.Pid))

	if c.server.pidfd == nil {
		t.Skip("pidfds not supported")
	}

	// Shutdown got past its check for Close, which runs then.
	_, release, err := c.usePidfd()
	if err != nil {
		t.Fatalf("use pidfd: %v", err)
	}

	if err := c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if err := c.stopServer(t.Context(), c.server, serverStopTimeout, false); err != nil {
		t.Fatalf("stop server: %v", err)
	}

	release()

	if !processExited(uint32(cmd.Process.Pid)) {
		t.Fatal("server still running after the shutdown")
	}

	if !errors.Is(c.server.pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd still open")
	}
}

func TestCloseWhileWaitingForServer(t *testing.T) {
	t.Parallel()

	cmd := startTestServer(t, true)

	logger, hook := logrustest.NewNullLogger()
	logger.SetLevel(logrus.DebugLevel)

	c := newTestClient(t, t.TempDir(), logger)
	c.setServerPID(uint32(cmd.Process.Pid))

	if c.server.pidfd == nil {
		t.Skip("pidfds not supported")
	}

	// The server ignores SIGINT, the shutdown waits for it until the
	// context ends.
	ctx, cancel := context.WithTimeout(t.Context(), time.Second)
	defer cancel()

	stopErr := make(chan error, 1)

	go func() { stopErr <- c.ShutdownContext(ctx) }()

	waitFor(t, "shutdown did not start", func() bool {
		c.pidfdMu.Lock()
		defer c.pidfdMu.Unlock()

		return c.server.users > 0
	})

	if err := c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if err := <-stopErr; !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("got error %v, want %v", err, context.DeadlineExceeded)
	}

	// The wait kept using the pidfd rather than the PID.
	for _, entry := range hook.AllEntries() {
		if strings.Contains(entry.Message, "pidfd") {
			t.Fatalf("pidfd not usable during the shutdown: %s: %v", entry.Message, entry.Data)
		}
	}

	// It got closed once the shutdown was over.
	if !errors.Is(c.server.pidfd.Close(), os.ErrClosed) {
		t.Fatal("pidfd still open")
	}
}

func TestConnectRunningServerIgnoresOtherSocketTypes(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// A packet socket, like the attach or fd sockets, which the server
	// replaces on start.
	listener, err := net.ListenUnix(
		"unixpacket",
		&net.UnixAddr{Name: filepath.Join(dir, socketName), Net: "unixpacket"},
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	t.Cleanup(func() { listener.Close() })

	c := newTestClient(t, dir, logrus.New())

	connected, err := c.connectRunningServer(t.Context())
	if err != nil || connected {
		t.Fatalf("got connected %v, error %v, want no server", connected, err)
	}
}

func TestNewRemovesStalePIDFile(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// The process which a PID file left behind names, like one which got
	// the PID of a server which is gone.
	stale := exec.CommandContext(t.Context(), "sleep", "60")
	if err := stale.Start(); err != nil {
		t.Fatalf("start process: %v", err)
	}

	t.Cleanup(func() {
		_ = stale.Process.Kill() //nolint:errcheck // best effort
		_ = stale.Wait()         //nolint:errcheck // killed
	})

	pidFile := filepath.Join(dir, pidFileName)
	if err := os.WriteFile(pidFile, []byte(strconv.Itoa(stale.Process.Pid)), 0o600); err != nil {
		t.Fatalf("write PID file: %v", err)
	}

	// A launch which exits without starting a server, like one which only
	// prints the version.
	binary := writeTestBinary(t, `exit 0`)

	ctx, cancel := context.WithTimeout(t.Context(), 500*time.Millisecond)
	defer cancel()

	_, err := NewContext(ctx, &ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: binary,
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
		timeouts:         shortTimeouts(),
	})
	if !errors.Is(err, ErrServerNotUp) {
		t.Fatalf("got error %v, want %v", err, ErrServerNotUp)
	}

	// A signal would have ended it, it does not trap any.
	if processExited(uint32(stale.Process.Pid)) {
		t.Fatal("process of the stale PID file got signaled")
	}

	if _, err := os.Stat(pidFile); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("PID file not removed: %v", err)
	}
}

func TestNewKeepsPIDFileOfServerNotStopped(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// A launch which writes a PID file that names no process, so that New
	// can neither wait for nor stop the server.
	binary := writeTestBinary(t, `printf invalid >"$4/pidfile"`)

	ctx, cancel := context.WithTimeout(t.Context(), 300*time.Millisecond)
	defer cancel()

	_, err := NewContext(ctx, &ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: binary,
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
		timeouts:         shortTimeouts(),
	})
	if !errors.Is(err, ErrServerNotUp) {
		t.Fatalf("got error %v, want %v", err, ErrServerNotUp)
	}

	got, err := os.ReadFile(filepath.Join(dir, pidFileName))
	if err != nil || string(got) != "invalid" {
		t.Fatalf("got PID file %q, error %v, want it kept", got, err)
	}
}

func TestNewDropsLaunchEnvironment(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// Records the variables and fails, so that New returns right away.
	binary := writeTestBinary(t, `printf 'version=%s json=%s fork=%s level=%s' `+
		`"${CONMON_VERSION-unset}" "${CONMON_VERSION_JSON-unset}" `+
		`"${CONMON_SKIP_FORK-unset}" "${CONMON_LOG_LEVEL-unset}" >"$4/env"; exit 1`)

	_, err := New(&ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: binary,
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
		Env: []string{
			"CONMON_VERSION=full",
			"CONMON_VERSION_JSON=full",
			"CONMON_SKIP_FORK=1",
			"CONMON_LOG_LEVEL=debug",
		},
	})
	if err == nil {
		t.Fatal("started a server which failed")
	}

	got, err := os.ReadFile(filepath.Join(dir, "env"))
	if err != nil {
		t.Fatalf("read environment: %v", err)
	}

	if want := "version=unset json=unset fork=unset level=debug"; string(got) != want {
		t.Fatalf("got environment %q, want %q", got, want)
	}
}

func TestNewReportsServerErrors(t *testing.T) {
	t.Parallel()

	const message = "Error: validate config: --runtime '/does/not/exist' does not exist"

	binary := writeTestBinary(t, `echo "`+message+`" >&2; exit 1`)

	for _, tc := range []struct {
		name   string
		driver LogDriver
		env    []string
		// stdout tells that the server gets the Stderr of the config.
		stdout bool
	}{
		{name: "default"},
		{name: "none", driver: LogDriverNone},
		{name: "systemd", driver: LogDriverSystemd},
		{name: "stdout", driver: LogDriverStdout, stdout: true},
		{name: "stdout of the environment", env: []string{"CONMON_LOG_DRIVER=stdout"}, stdout: true},
		{
			name:   "config over environment",
			driver: LogDriverNone,
			env:    []string{"CONMON_LOG_DRIVER=stdout"},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			stderr, err := os.Create(filepath.Join(t.TempDir(), "stderr"))
			if err != nil {
				t.Fatalf("create stderr: %v", err)
			}

			defer stderr.Close()

			_, err = New(&ConmonServerConfig{
				ServerRunDir:     t.TempDir(),
				ConmonServerPath: binary,
				Runtime:          "/does/not/exist",
				LogDriver:        tc.driver,
				Env:              tc.env,
				Stderr:           stderr,
				ClientLogger:     logrus.New(),
			})
			if err == nil {
				t.Fatal("started a server which failed")
			}

			// The stdout driver passes stderr to the writer instead.
			stdout := tc.stdout
			if strings.Contains(err.Error(), message) == stdout {
				t.Fatalf("got error %q, want the message of the server: %v", err, !stdout)
			}

			written, err := os.ReadFile(stderr.Name())
			if err != nil {
				t.Fatalf("read stderr: %v", err)
			}

			if strings.Contains(string(written), message) != stdout {
				t.Fatalf("got stderr %q, want the message of the server: %v", written, stdout)
			}
		})
	}
}

func TestReadServerErrors(t *testing.T) {
	t.Parallel()

	if got := readServerErrors(nil); got != "" {
		t.Fatalf("got %q without a file", got)
	}

	file, err := serverErrorFile()
	if err != nil {
		t.Fatalf("create error file: %v", err)
	}

	defer file.Close()

	if got := readServerErrors(file); got != "" {
		t.Fatalf("got %q from an empty file", got)
	}

	// Written like the server does, which moves the shared offset.
	if _, err := file.WriteString("first\n"); err != nil {
		t.Fatalf("write: %v", err)
	}

	if got := readServerErrors(file); got != "first" {
		t.Fatalf("got %q, want %q", got, "first")
	}

	// Only the end of a long output, which has the error.
	if _, err := file.WriteString(strings.Repeat("x", maxServerErrors) + "last\n"); err != nil {
		t.Fatalf("write: %v", err)
	}

	got := readServerErrors(file)
	if !strings.HasPrefix(got, "...") || !strings.HasSuffix(got, "last") ||
		len(got) > maxServerErrors+len("...") {
		t.Fatalf(
			"got %d bytes %.20q...%q, want the bounded end",
			len(got),
			got,
			got[max(len(got)-10, 0):],
		)
	}
}

func TestSetServerOutput(t *testing.T) {
	t.Parallel()

	c := &ConmonClient{logger: logrus.New()}

	file, err := os.Create(filepath.Join(t.TempDir(), "output"))
	if err != nil {
		t.Fatalf("create file: %v", err)
	}

	t.Cleanup(func() { file.Close() })

	shared := make(chanWriter, 2)

	for _, tc := range []struct {
		name           string
		stdout, stderr io.Writer
		// wantStdout and wantStderr are the files the command gets, nil for
		// a pipe.
		wantStdout, wantStderr *os.File
		// samePipe tells that both get the same pipe.
		samePipe bool
	}{
		{
			name:       "defaults",
			wantStdout: os.Stdout,
			wantStderr: os.Stderr,
		},
		{
			name:       "files",
			stdout:     file,
			stderr:     file,
			wantStdout: file,
			wantStderr: file,
		},
		{
			name:       "writer and file",
			stdout:     make(chanWriter),
			stderr:     file,
			wantStderr: file,
		},
		{
			name:     "same writer",
			stdout:   shared,
			stderr:   shared,
			samePipe: true,
		},
		{
			name:   "other writers",
			stdout: make(chanWriter),
			stderr: make(chanWriter),
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			cmd := exec.CommandContext(t.Context(), "true")

			release, outputErr := c.setServerOutput(cmd, &ConmonServerConfig{
				Stdout: tc.stdout,
				Stderr: tc.stderr,
			})
			if outputErr != nil {
				t.Fatalf("set server output: %v", outputErr)
			}

			defer release()

			for _, out := range []struct {
				name string
				got  io.Writer
				want *os.File
			}{
				{"stdout", cmd.Stdout, tc.wantStdout},
				{"stderr", cmd.Stderr, tc.wantStderr},
			} {
				got, ok := out.got.(*os.File)
				if !ok {
					t.Fatalf("%s is no file: %T", out.name, out.got)
				}

				if out.want != nil && got != out.want {
					t.Errorf("%s got %s, want %s", out.name, got.Name(), out.want.Name())
				}

				if out.want == nil && (got == os.Stdout || got == os.Stderr || got == file) {
					t.Errorf("%s got %s, want a pipe", out.name, got.Name())
				}
			}

			if pipes := tc.wantStdout == nil && tc.wantStderr == nil; pipes &&
				(cmd.Stdout == cmd.Stderr) != tc.samePipe {
				t.Errorf("got the same pipe %v, want %v", cmd.Stdout == cmd.Stderr, tc.samePipe)
			}
		})
	}

	// Both streams end up in the shared writer through the one pipe, in the
	// order of the writes.
	cmd := exec.CommandContext(t.Context(), "sh", "-c", "printf out; printf err >&2")

	release, err := c.setServerOutput(cmd, &ConmonServerConfig{Stdout: shared, Stderr: shared})
	if err != nil {
		t.Fatalf("set server output: %v", err)
	}

	if err := cmd.Run(); err != nil {
		t.Fatalf("run command: %v", err)
	}

	release()

	// The copy may get both writes with one read.
	const want = "outerr"

	for got := ""; got != want; {
		select {
		case chunk := <-shared:
			got += chunk
			if !strings.HasPrefix(want, got) {
				t.Fatalf("got output %q, want %q", got, want)
			}
		case <-time.After(10 * time.Second):
			t.Fatalf("got output %q, timed out waiting for %q", got, want)
		}
	}
}

func TestConnectRunningServerFullBacklog(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()

	// A socket which nothing accepts on, with the smallest backlog.
	fd := bindTestSocket(t, dir)

	if err := syscall.Listen(fd, 0); err != nil {
		t.Fatalf("listen: %v", err)
	}

	// Fill the backlog until a dial fails like the one of New does.
	for range 64 {
		conn, dialErr := dialLongSocket("unix", filepath.Join(dir, socketName))
		if errors.Is(dialErr, syscall.EAGAIN) {
			break
		}

		if dialErr != nil {
			t.Fatalf("dial: %v", dialErr)
		}

		t.Cleanup(func() { conn.Close() })
	}

	c := newTestClient(t, dir, logrus.New())

	connected, err := c.connectRunningServer(t.Context())
	if connected || !errors.Is(err, ErrServerNotResponding) || !errors.Is(err, syscall.EAGAIN) {
		t.Fatalf("got connected %v, error %v, want %v", connected, err, ErrServerNotResponding)
	}
}
