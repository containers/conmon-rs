package client

import (
	"context"
	"errors"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
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

			conns = append(
				conns,
				rpc.NewConn(rpc.NewStreamTransport(socket), serverOptions(withBootstrap)),
			)
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
