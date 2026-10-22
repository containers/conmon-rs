package client

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
	"go.podman.io/storage/pkg/idtools"
)

// fakeConmon is a server whose methods are never called.
type fakeConmon struct {
	proto.Conmon_Server
}

// serverOptions returns the options of a server peer. Without a bootstrap
// capability, the bootstrap of the client fails.
func serverOptions(withBootstrap bool) *rpc.Options {
	if !withBootstrap {
		return nil
	}

	return &rpc.Options{BootstrapClient: capnp.Client(proto.Conmon_ServerToClient(fakeConmon{}))}
}

// newTestConn returns a connection with a peer, the socket of the peer and
// the resolved bootstrap capability, everything gets closed with the test.
func newTestConn(t *testing.T, withBootstrap bool) (*rpcConn, net.Conn) {
	t.Helper()

	local, remote := net.Pipe()
	conn := newRPCConn(t.Context(), local)
	peer := rpc.NewConn(rpc.NewStreamTransport(remote), serverOptions(withBootstrap))

	t.Cleanup(func() {
		conn.conn.Close()
		peer.Close()
	})

	if err := capnp.Client(conn.client).Resolve(t.Context()); err != nil {
		t.Fatalf("resolve bootstrap: %v", err)
	}

	return conn, remote
}

// waitBroken waits until the connection got marked as broken.
func waitBroken(t *testing.T, conn *rpcConn) {
	t.Helper()

	for deadline := time.Now().Add(10 * time.Second); !conn.broken.Load(); {
		if time.Now().After(deadline) {
			t.Fatal("connection not marked as broken")
		}

		time.Sleep(time.Millisecond)
	}
}

func TestRPCConnUsable(t *testing.T) {
	t.Parallel()

	conn, _ := newTestConn(t, true)
	if !conn.usable() {
		t.Error("open connection with a bootstrap capability unusable")
	}

	// The failed bootstrap gets recorded once, the check only reads it.
	conn, _ = newTestConn(t, false)
	waitBroken(t, conn)

	if conn.usable() {
		t.Error("open connection with a failed bootstrap usable")
	}

	conn, _ = newTestConn(t, true)
	conn.conn.Close()

	if conn.usable() {
		t.Error("closed connection usable")
	}
}

func TestRPCConnUnusableOnceServerGone(t *testing.T) {
	t.Parallel()

	conn, remote := newTestConn(t, true)

	// The server exits, a call fails with a disconnect. A retry right after
	// it must not get the connection again, although its shutdown may still
	// be in progress.
	remote.Close()

	future, release := conn.client.Version(t.Context(), nil)
	_, err := future.Struct()

	release()

	if err == nil {
		t.Fatal("call succeeded without a server")
	}

	if conn.usable() {
		t.Errorf("connection usable after a call failed with %v", err)
	}
}

// serveTestSocket serves RPC connections on the socket of a client in dir.
func serveTestSocket(t *testing.T, dir string, withBootstrap bool) {
	t.Helper()

	serveSocket(t, dir, func() *rpc.Options { return serverOptions(withBootstrap) })
}

// serveSocket serves RPC connections with the options on the socket of a
// client in dir.
func serveSocket(t *testing.T, dir string, options func() *rpc.Options) {
	t.Helper()

	listener, err := (&net.ListenConfig{}).Listen(
		t.Context(),
		"unix",
		filepath.Join(dir, socketName),
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	// Only read once the accepting goroutine is done.
	var conns []*rpc.Conn

	done := make(chan struct{})

	go func() {
		defer close(done)

		for {
			socket, err := listener.Accept()
			if err != nil {
				return
			}

			conns = append(conns, rpc.NewConn(rpc.NewStreamTransport(socket), options()))
		}
	}()

	t.Cleanup(func() {
		listener.Close()
		<-done

		for _, conn := range conns {
			conn.Close()
		}
	})
}

// versionConmon is a server which only answers Version.
type versionConmon struct {
	proto.Conmon_Server

	pid      uint32
	features []string

	// called gets notified of every call, if set.
	called chan<- struct{}

	// release holds the answers back until it gets closed, if set.
	release <-chan struct{}
}

func (v versionConmon) Version(ctx context.Context, call proto.Conmon_version) error {
	if v.called != nil {
		select {
		case v.called <- struct{}{}:
		default:
		}
	}

	if v.release != nil {
		select {
		case <-v.release:
		case <-ctx.Done():
			return fmt.Errorf("wait for release: %w", ctx.Err())
		}
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	response, err := results.NewResponse()
	if err != nil {
		return fmt.Errorf("new response: %w", err)
	}

	response.SetProcessId(v.pid)

	features, err := response.NewFeatures(int32(len(v.features)))
	if err != nil {
		return fmt.Errorf("new features: %w", err)
	}

	for i, feature := range v.features {
		if err := features.Set(i, feature); err != nil {
			return fmt.Errorf("set feature: %w", err)
		}
	}

	return nil
}

// serveVersionSocket serves the Version answers of the server on the
// socket of a client in dir.
func serveVersionSocket(t *testing.T, dir string, server versionConmon) {
	t.Helper()

	serveSocket(t, dir, func() *rpc.Options {
		return &rpc.Options{BootstrapClient: capnp.Client(proto.Conmon_ServerToClient(server))}
	})
}

// serveSilentSocket listens on the socket of the run directory and accepts
// connections without ever answering on them, like a hanging server.
func serveSilentSocket(t *testing.T, dir string) {
	t.Helper()

	listener, err := (&net.ListenConfig{}).Listen(
		t.Context(),
		"unix",
		filepath.Join(dir, socketName),
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	// Only read once the accepting goroutine is done.
	var conns []net.Conn

	done := make(chan struct{})

	go func() {
		defer close(done)

		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}

			conns = append(conns, conn)
		}
	}()

	t.Cleanup(func() {
		listener.Close()
		<-done

		for _, conn := range conns {
			conn.Close()
		}
	})
}

func TestNewKeepsServerWhichDoesNotAnswer(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	serveSilentSocket(t, dir)

	start := time.Now()
	_, err := New(&ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: "/does/not/exist",
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
	})

	if !errors.Is(err, ErrServerNotResponding) {
		t.Fatalf("got error %v, want %v", err, ErrServerNotResponding)
	}

	// The listener runs in this process.
	if pid := "PID " + strconv.Itoa(os.Getpid()); !strings.Contains(err.Error(), pid) {
		t.Fatalf("error %q does not name %s", err, pid)
	}

	if elapsed := time.Since(start); elapsed > serverProbeTimeout+time.Second {
		t.Fatalf("probe took %s, want about %s", elapsed, serverProbeTimeout)
	}

	if _, err := os.Stat(filepath.Join(dir, socketName)); err != nil {
		t.Fatalf("socket got removed: %v", err)
	}
}

// serveClosingSocket listens on the socket of the run directory and closes
// every connection right away, like a server which shuts down. It returns a
// function which stops listening.
func serveClosingSocket(t *testing.T, dir string) func() {
	t.Helper()

	listener, err := (&net.ListenConfig{}).Listen(
		t.Context(),
		"unix",
		filepath.Join(dir, socketName),
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}

			conn.Close()
		}
	}()

	stop := sync.OnceFunc(func() {
		listener.Close()
		<-done
	})
	t.Cleanup(stop)

	return stop
}

func TestNewReplacesServerWhichGoesAway(t *testing.T) {
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

	// A server which shuts down and stops accepting a bit later. Without a
	// server binary, the start fails, which shows that New tried.
	dir := t.TempDir()
	stop := serveClosingSocket(t, dir)

	time.AfterFunc(500*time.Millisecond, stop)

	if err := newClient(dir); err == nil || errors.Is(err, ErrServerNotResponding) ||
		!strings.Contains(err.Error(), "start server") {
		t.Fatalf("got error %v, want a failed server start", err)
	}

	// One which keeps accepting is not replaced.
	dir = t.TempDir()
	serveClosingSocket(t, dir)

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

	listener, err := net.ListenUnix(
		"unix",
		&net.UnixAddr{Name: filepath.Join(stale, socketName), Net: "unix"},
	)
	if err != nil {
		t.Fatalf("listen: %v", err)
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

	_, err = cl.connectRunningServer(t.Context())
	if !errors.Is(err, os.ErrPermission) || errors.Is(err, ErrServerNotResponding) {
		t.Fatalf("got error %v, want a permission error", err)
	}
}

func TestClientReplacesUnusableConnection(t *testing.T) {
	t.Parallel()

	for _, withBootstrap := range []bool{true, false} {
		t.Run("bootstrap "+strconv.FormatBool(withBootstrap), func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			serveTestSocket(t, dir, withBootstrap)

			c := &ConmonClient{runDir: dir, logger: logrus.New()}
			t.Cleanup(c.resetConn)

			if _, err := c.client(t.Context()); err != nil {
				t.Fatalf("create client: %v", err)
			}

			first := c.conn

			if withBootstrap {
				if err := capnp.Client(first.client).Resolve(t.Context()); err != nil {
					t.Fatalf("resolve bootstrap: %v", err)
				}
			} else {
				waitBroken(t, first)
			}

			if _, err := c.client(t.Context()); err != nil {
				t.Fatalf("create client again: %v", err)
			}

			// A failed bootstrap makes the connection unusable for good.
			if replaced := c.conn != first; replaced == withBootstrap {
				t.Fatalf("connection replaced: %v, want %v", replaced, !withBootstrap)
			}

			if !withBootstrap {
				return
			}

			// A closed connection gets replaced as well.
			c.conn.conn.Close()

			if _, err := c.client(t.Context()); err != nil {
				t.Fatalf("create client after close: %v", err)
			}

			if c.conn == first {
				t.Fatal("closed connection not replaced")
			}
		})
	}
}

func TestNamespaceConversion(t *testing.T) {
	t.Parallel()

	for _, namespace := range []Namespace{NamespaceIPC, NamespacePID, NamespaceNet, NamespaceUser, NamespaceUTS} {
		typ, err := namespaceTypes.toProto(namespace)
		if err != nil {
			t.Fatalf("convert namespace %d: %v", namespace, err)
		}

		back, err := namespaceTypes.fromProto(typ)
		if err != nil {
			t.Fatalf("convert namespace %v back: %v", typ, err)
		}

		if back != namespace {
			t.Errorf("namespace %d converted back to %d", namespace, back)
		}
	}

	if _, err := namespaceTypes.toProto(Namespace(42)); !errors.Is(err, errInvalidValue) {
		t.Errorf("unknown namespace: got %v, want %v", err, errInvalidValue)
	}

	if _, err := namespaceTypes.fromProto(
		proto.Conmon_Namespace(42),
	); !errors.Is(
		err,
		errInvalidValue,
	) {
		t.Errorf("unknown namespace from server: got %v, want %v", err, errInvalidValue)
	}
}

func TestNamespacesToProto(t *testing.T) {
	t.Parallel()

	mappings := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

	for _, tc := range []struct {
		name string
		cfg  *CreateNamespacesConfig
		err  error
	}{
		{"none", &CreateNamespacesConfig{}, nil},
		{"user with mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceNet, NamespaceUser},
			IDMappings: idtools.NewIDMappingsFromMaps(mappings, mappings),
		}, nil},
		{"user without mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceUser},
		}, ErrMissingIDMappings},
		{"user without GID mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceUser},
			IDMappings: idtools.NewIDMappingsFromMaps(mappings, nil),
		}, ErrMissingIDMappings},
		{"unknown", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceIPC, Namespace(-1)},
		}, errInvalidValue},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			namespaces, err := namespacesToProto(tc.cfg)
			if !errors.Is(err, tc.err) {
				t.Fatalf("got error %v, want %v", err, tc.err)
			}

			if err == nil && len(namespaces) != len(tc.cfg.Namespaces) {
				t.Errorf("got %d namespaces, want %d", len(namespaces), len(tc.cfg.Namespaces))
			}
		})
	}
}

func TestLogDriverTypes(t *testing.T) {
	t.Parallel()

	for typ, want := range map[LogDriverType]proto.Conmon_LogDriver_Type{
		LogDriverTypeContainerRuntimeInterface: proto.Conmon_LogDriver_Type_containerRuntimeInterface,
		LogDriverTypeJSONLogger:                proto.Conmon_LogDriver_Type_json,
		LogDriverTypeJournald:                  proto.Conmon_LogDriver_Type_journald,
	} {
		got, err := logDriverTypes.toProto(typ)
		if err != nil || got != want {
			t.Errorf("type %d: got %v, %v, want %v", typ, got, err, want)
		}
	}

	logDrivers, err := logDriversToProto([]ContainerLogDriver{
		{Type: LogDriverTypeJournald}, {Type: LogDriverTypeJSONLogger, Path: "log", MaxSize: 1},
	})
	if err != nil {
		t.Fatalf("convert log drivers: %v", err)
	}

	want := []protoLogDriver{
		{typ: proto.Conmon_LogDriver_Type_journald},
		{typ: proto.Conmon_LogDriver_Type_json, path: "log", maxSize: 1},
	}
	if !slices.Equal(logDrivers, want) {
		t.Errorf("got log drivers %+v, want %+v", logDrivers, want)
	}

	if _, err := logDriversToProto([]ContainerLogDriver{
		{Type: LogDriverTypeJournald}, {Type: LogDriverType(42)},
	}); !errors.Is(err, errInvalidValue) {
		t.Errorf("unknown type: got %v, want %v", err, errInvalidValue)
	}
}

func TestExecTimeout(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name     string
		deadline time.Duration // none if 0
		timeout  uint64
		want     uint64
	}{
		{"explicit timeout below deadline", time.Hour, 5, 5},
		{"explicit timeout limited by deadline", 2500 * time.Millisecond, 3600, 2},
		{"explicit timeout without deadline", 0, 7, 7},
		{"no deadline", 0, 0, 0},
		{"deadline rounded down", 2500 * time.Millisecond, 0, 2},
		{"short deadline", 500 * time.Millisecond, 0, 1},
		{"expired deadline", -time.Second, 0, 1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			ctx := context.Background()

			if tc.deadline != 0 {
				var cancel context.CancelFunc

				ctx, cancel = context.WithTimeout(ctx, tc.deadline)
				defer cancel()
			}

			if got := execTimeout(ctx, tc.timeout); got != tc.want {
				t.Errorf("got %d, want %d", got, tc.want)
			}
		})
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
	deadline := time.Now().Add(10 * time.Second)
	for !processExited(pid) {
		if time.Now().After(deadline) {
			t.Fatal("zombie not reported as exited")
		}

		time.Sleep(10 * time.Millisecond)
	}

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
		deadline := time.Now().Add(10 * time.Second)

		for {
			comm, err := os.ReadFile("/proc/" + strconv.Itoa(cmd.Process.Pid) + "/comm")
			if err == nil && strings.TrimSpace(string(comm)) == "sleep" {
				break
			}

			if time.Now().After(deadline) {
				t.Fatal("test server did not exec")
			}

			time.Sleep(10 * time.Millisecond)
		}
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

				if !withPidfd && c.serverPidfd != nil {
					c.serverPidfd.Close()
					c.serverPidfd = nil
				} else if withPidfd && c.serverPidfd == nil {
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
				200*time.Millisecond,
				false,
			); !errors.Is(
				err,
				errTimeoutWaitForPid,
			) {
				t.Fatalf("got error %v, want %v", err, errTimeoutWaitForPid)
			}

			if err := c.stopServer(t.Context(), 200*time.Millisecond, true); err != nil {
				t.Fatalf("stop with kill: %v", err)
			}

			if !processExited(uint32(cmd.Process.Pid)) {
				t.Fatal("server still running after the kill")
			}

			// The server is gone for good, its pidfd got released.
			if !c.serverDone.Load() {
				t.Fatal("server not marked as gone")
			}

			if withPidfd && !errors.Is(c.serverPidfd.Close(), os.ErrClosed) {
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

	if c.serverPidfd == nil {
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

	old := c.serverPidfd
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

	if c.serverPidfd != nil {
		t.Skip("PID got reused")
	}

	if !c.serverDone.Load() {
		t.Fatal("exited server not marked as gone")
	}

	if err := c.signalServer(syscall.SIGINT); !errors.Is(err, syscall.ESRCH) {
		t.Fatalf("got error %v, want %v", err, syscall.ESRCH)
	}

	if err := c.Shutdown(); err != nil {
		t.Fatalf("shutdown: %v", err)
	}
}

// pidFromFile waits for the PID file of a test server binary in dir.
func pidFromFile(t *testing.T, dir string) int {
	t.Helper()

	deadline := time.Now().Add(10 * time.Second)

	for {
		data, err := os.ReadFile(filepath.Join(dir, pidFileName))
		if pid, convErr := strconv.Atoi(strings.TrimSpace(string(data))); err == nil &&
			convErr == nil {
			return pid
		}

		if time.Now().After(deadline) {
			t.Fatal("no PID file")
		}

		time.Sleep(10 * time.Millisecond)
	}
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
	// --runtime, the runtime, --runtime-dir and the run directory.
	binary := writeTestBinary(
		t,
		`(trap '' INT; exec sleep 60) </dev/null >/dev/null 2>&1 & printf %s $! >"$4/pidfile"`,
	)

	ctx, cancel := context.WithTimeout(t.Context(), 500*time.Millisecond)
	defer cancel()

	start := time.Now()

	_, err := NewContext(ctx, &ConmonServerConfig{
		ServerRunDir:     dir,
		ConmonServerPath: binary,
		Runtime:          "/does/not/exist",
		ClientLogger:     logrus.New(),
	})
	if !errors.Is(err, errServerNotUp) || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("got error %v, want %v and %v", err, errServerNotUp, context.DeadlineExceeded)
	}

	pid := pidFromFile(t, dir)
	killOnCleanup(t, pid)

	if !processExited(uint32(pid)) {
		t.Fatal("server still running")
	}

	// SIGINT did not end it, SIGKILL did.
	if elapsed := time.Since(start); elapsed < serverCleanupTimeout {
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
	if !cl.serverFeatures()[FeatureSdNotify] {
		t.Fatal("features not recorded")
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

func TestRequireServerFeatureDoesNotBlock(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	called := make(chan struct{}, 1)
	release := make(chan struct{})

	serveVersionSocket(t, dir, versionConmon{
		features: []string{FeaturePortForward},
		called:   called,
		release:  release,
	})

	c, err := (&ConmonServerConfig{ServerRunDir: dir, ClientLogger: logrus.New()}).toClient()
	if err != nil {
		t.Fatalf("client: %v", err)
	}

	t.Cleanup(c.resetConn)

	slow := make(chan error, 1)

	go func() {
		slow <- c.requireServerFeature(t.Context(), FeaturePortForward, "test")
	}()

	<-called

	// A second caller asks the server as well, but only waits as long as its
	// own context allows.
	ctx, cancel := context.WithTimeout(t.Context(), 100*time.Millisecond)
	defer cancel()

	start := time.Now()

	err = c.requireServerFeature(ctx, FeaturePortForward, "test")
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("got error %v, want %v", err, context.DeadlineExceeded)
	}

	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("second caller blocked for %s", elapsed)
	}

	close(release)

	if err := <-slow; err != nil {
		t.Fatalf("slow caller: %v", err)
	}

	if !c.serverFeatures()[FeaturePortForward] {
		t.Fatal("features not recorded")
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

	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}

	// A zero PID would signal the process group of the caller.
	if err := c.Shutdown(); !errors.Is(err, errServerPIDUnknown) {
		t.Fatalf("got error %v, want %v", err, errServerPIDUnknown)
	}
}

func TestRequireServerFeature(t *testing.T) {
	t.Parallel()

	// Recorded features need no server.
	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	c.setFeatures([]string{FeatureSdNotify})

	if err := c.requireServerFeature(t.Context(), FeatureSdNotify, "test"); err != nil {
		t.Fatalf("recorded feature: %v", err)
	}

	if err := c.requireServerFeature(
		t.Context(),
		FeaturePortForward,
		"test",
	); !errors.Is(
		err,
		ErrUnsupported,
	) {
		t.Fatalf("got error %v, want %v", err, ErrUnsupported)
	}
}

func TestNewInvalidConfig(t *testing.T) {
	t.Parallel()

	if _, err := New(nil); !errors.Is(err, errConfigUnspecified) {
		t.Fatalf("got error %v, want %v", err, errConfigUnspecified)
	}

	if _, err := New(&ConmonServerConfig{}); !errors.Is(err, errRunDirUnspecified) {
		t.Fatalf("got error %v, want %v", err, errRunDirUnspecified)
	}

	// The cause of a run directory which cannot be created is kept.
	file := filepath.Join(t.TempDir(), "file")
	if err := os.WriteFile(file, nil, 0o600); err != nil {
		t.Fatalf("write file: %v", err)
	}

	_, err := New(&ConmonServerConfig{ServerRunDir: filepath.Join(file, "run")})
	if !errors.Is(err, errRunDirNotCreated) || !errors.Is(err, syscall.ENOTDIR) {
		t.Fatalf("got error %v, want %v and %v", err, errRunDirNotCreated, syscall.ENOTDIR)
	}
}

// writeTestBinary writes a shell script which stands in for the server
// binary.
func writeTestBinary(t *testing.T, script string) string {
	t.Helper()

	path := filepath.Join(t.TempDir(), "conmonrs")
	//nolint:gosec // has to be executable
	if err := os.WriteFile(path, []byte("#!/bin/sh\n"+script+"\n"), 0o700); err != nil {
		t.Fatalf("write binary: %v", err)
	}

	return path
}

func TestVersionBinary(t *testing.T) {
	t.Parallel()

	// Output on stderr does not end up in the JSON.
	binary := writeTestBinary(t, `echo warning >&2; echo '{"version":"1.2.3"}'`)

	version, err := Version(binary)
	if err != nil {
		t.Fatalf("version: %v", err)
	}

	if version.Version != "1.2.3" {
		t.Fatalf("got version %q, want %q", version.Version, "1.2.3")
	}

	binary = writeTestBinary(
		t,
		`echo "error: unexpected argument '--version-json' found" >&2; exit 2`,
	)

	if _, err := Version(binary); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("got error %v, want %v", err, ErrUnsupported)
	}

	binary = writeTestBinary(t, `echo broken >&2; exit 1`)

	if _, err := Version(binary); err == nil || !strings.Contains(err.Error(), "broken") {
		t.Fatalf("got error %v, want one with the error output", err)
	}
}
