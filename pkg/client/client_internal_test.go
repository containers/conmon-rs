package client

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/exc"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
	logrustest "github.com/sirupsen/logrus/hooks/test"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
	"go.opentelemetry.io/otel/trace/noop"
	"go.podman.io/common/pkg/detach"
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

// waitFor polls the condition until it holds, the test fails with the
// message if it does not within 10 seconds.
func waitFor(t *testing.T, message string, condition func() bool) {
	t.Helper()

	for deadline := time.Now().Add(10 * time.Second); !condition(); {
		if time.Now().After(deadline) {
			t.Fatal(message)
		}

		time.Sleep(time.Millisecond)
	}
}

// waitBroken waits until the connection got marked as broken.
func waitBroken(t *testing.T, conn *rpcConn) {
	t.Helper()

	waitFor(t, "connection not marked as broken", conn.broken.Load)
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
// client in dir, until the test or the returned stop ends.
func serveSocket(t *testing.T, dir string, options func() *rpc.Options) (stop func()) {
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

	var once sync.Once

	stop = func() {
		once.Do(func() {
			listener.Close()
			<-done

			for _, conn := range conns {
				conn.Close()
			}
		})
	}

	t.Cleanup(stop)

	return stop
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
// socket of a client in dir, see serveSocket.
func serveVersionSocket(t *testing.T, dir string, server proto.Conmon_Server) (stop func()) {
	t.Helper()

	return serveSocket(t, dir, func() *rpc.Options {
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

	labels := map[string]string{"POD": "web"}

	logDrivers, err := logDriversToProto([]ContainerLogDriver{
		{Type: LogDriverTypeJournald, ContainerName: "name", Tag: "tag", Labels: labels},
		{Type: LogDriverTypeJSONLogger, Path: "log", MaxSize: 1},
	})
	if err != nil {
		t.Fatalf("convert log drivers: %v", err)
	}

	want := []protoLogDriver{
		{
			typ:           proto.Conmon_LogDriver_Type_journald,
			containerName: "name",
			tag:           "tag",
			labels:        labels,
		},
		{typ: proto.Conmon_LogDriver_Type_json, path: "log", maxSize: 1},
	}
	if !reflect.DeepEqual(logDrivers, want) {
		t.Errorf("got log drivers %+v, want %+v", logDrivers, want)
	}

	// The request carries the fields of the journald driver.
	_, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	req, err := proto.NewRootConmon_CreateContainerRequest(seg)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}

	err = initLogDrivers(&req, logDrivers)
	if err != nil {
		t.Fatalf("init log drivers: %v", err)
	}

	list, err := req.LogDrivers()
	if err != nil {
		t.Fatalf("log drivers: %v", err)
	}

	journald := list.At(0)

	entries, err := journald.Labels()
	if err != nil || entries.Len() != 1 {
		t.Fatalf("labels: %v, %d entries", err, entries.Len())
	}

	texts := []func() (string, error){
		journald.ContainerName, journald.Tag, entries.At(0).Key, entries.At(0).Value,
	}
	got := make([]string, 0, len(texts))

	for _, text := range texts {
		value, textErr := text()
		if textErr != nil {
			t.Fatalf("read text: %v", textErr)
		}

		got = append(got, value)
	}

	if want := []string{"name", "tag", "POD", "web"}; !slices.Equal(got, want) {
		t.Errorf("got name, tag and label %q, want %q", got, want)
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

// pidFromFile waits for the PID file of a test server binary in dir.
func pidFromFile(t *testing.T, dir string) int {
	t.Helper()

	var pid int

	waitFor(t, "no PID file", func() bool {
		data, err := os.ReadFile(filepath.Join(dir, pidFileName))
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

func TestSupportsFeature(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	c := newTestClient(t, dir, logrus.New())

	t.Cleanup(func() { c.Close() })

	if c.SupportsFeature(FeatureSdNotify) {
		t.Fatal("feature supported before connecting")
	}

	for i, features := range [][]string{
		{FeatureSdNotify, FeatureStreamingProtocols},
		// Another server, which replaced the first one.
		{FeatureSdNotify},
	} {
		if i > 0 {
			if err := os.Remove(c.socket()); err != nil {
				t.Fatalf("remove socket: %v", err)
			}
		}

		serveVersionSocket(t, dir, versionConmon{features: features})

		connected, err := c.connectRunningServer(t.Context())
		if err != nil || !connected {
			t.Fatalf("connect: %v, connected %v", err, connected)
		}

		for _, feature := range []string{
			FeatureSdNotify,
			FeaturePortForward,
			FeatureIdleTimeout,
			FeatureStreamingProtocols,
		} {
			want := slices.Contains(features, feature)
			if got := c.SupportsFeature(feature); got != want {
				t.Fatalf("server with %v: feature %q supported %v", features, feature, got)
			}
		}
	}
}

// reopeningConmon answers Version with its features and ReopenLogContainer,
// its Version fails with versionErr if set.
type reopeningConmon struct {
	proto.Conmon_Server

	features   []string
	versionErr error
}

func (r reopeningConmon) Version(ctx context.Context, call proto.Conmon_version) error {
	if r.versionErr != nil {
		return r.versionErr
	}

	return versionConmon{features: r.features}.Version(ctx, call)
}

func (reopeningConmon) ReopenLogContainer(
	_ context.Context,
	call proto.Conmon_reopenLogContainer,
) error {
	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	if _, err := results.NewResponse(); err != nil {
		return fmt.Errorf("new response: %w", err)
	}

	return nil
}

func TestSupportsFeatureAfterReconnect(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	stop := serveVersionSocket(t, dir, versionConmon{
		features: []string{FeatureSdNotify, FeatureStreamingProtocols},
	})

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	connected, err := c.connectRunningServer(t.Context())
	if err != nil || !connected {
		t.Fatalf("connect: %v, connected %v", err, connected)
	}

	for _, server := range []reopeningConmon{
		// A server which does not tell its features keeps the recorded
		// ones, the call which reconnected still succeeds.
		//nolint:err113 // any failure of Version
		{versionErr: errors.New("version failed")},
		{features: []string{FeatureSdNotify}},
	} {
		// Another server replaces the one the client is connected to.
		conn := c.conn

		stop()
		waitBroken(t, conn)

		stop = serveVersionSocket(t, dir, server)

		err := c.ReopenLogContainer(t.Context(), &ReopenLogContainerConfig{ID: "id"})
		if err != nil {
			t.Fatalf("reopen log after reconnect: %v", err)
		}

		if c.conn == conn {
			t.Fatal("connection not replaced")
		}

		want := server.versionErr != nil
		if got := c.SupportsFeature(FeatureStreamingProtocols); got != want {
			t.Fatalf("server with %v: feature supported %v, want %v", server.features, got, want)
		}

		if !c.SupportsFeature(FeatureSdNotify) {
			t.Fatalf("server with %v: feature %q not supported", server.features, FeatureSdNotify)
		}
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

func TestNewInvalidConfig(t *testing.T) {
	t.Parallel()

	if _, err := New(nil); !errors.Is(err, ErrMissingConfig) {
		t.Fatalf("got error %v, want %v", err, ErrMissingConfig)
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

// failingConmon is a server which fails ReopenLogContainer and
// ServeExecContainer with its error.
type failingConmon struct {
	proto.Conmon_Server

	err error
}

func (f failingConmon) ReopenLogContainer(context.Context, proto.Conmon_reopenLogContainer) error {
	return f.err
}

func (f failingConmon) ServeExecContainer(context.Context, proto.Conmon_serveExecContainer) error {
	return f.err
}

func TestMatchedServerErrors(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		message string
		// The exported error the failure matches, if any.
		target error
	}{
		{"container not found: unknown", ErrContainerNotFound},
		// The message of older servers.
		{"child not available", ErrContainerNotFound},
		{"open log file: permission denied", nil},
		// Only the start of the message counts.
		{"create container: runtime: container not found", nil},
		{"maximum number of in-flight requests exceeded", ErrTooManyRequests},
		{"exec: maximum number of in-flight requests exceeded", nil},
	} {
		t.Run(tc.message, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			//nolint:err113 // the message of the server matters
			server := failingConmon{err: errors.New(tc.message)}

			serveSocket(t, dir, func() *rpc.Options {
				return &rpc.Options{
					BootstrapClient: capnp.Client(proto.Conmon_ServerToClient(server)),
				}
			})

			c := &ConmonClient{runDir: dir, logger: logrus.New()}
			t.Cleanup(c.resetConn)

			for name, call := range map[string]func() error{
				"ReopenLogContainer": func() error {
					return c.ReopenLogContainer(t.Context(), &ReopenLogContainerConfig{ID: "unknown"})
				},
				"ServeExecContainer": func() error {
					_, err := c.ServeExecContainer(t.Context(), &ServeExecContainerConfig{ID: "unknown"})

					return err
				},
			} {
				err := call()
				if err == nil {
					t.Fatalf("%s succeeded", name)
				}

				for _, target := range []error{ErrContainerNotFound, ErrTooManyRequests} {
					if want := errors.Is(target, tc.target); errors.Is(err, target) != want {
						t.Fatalf("%s: error %v matches %v: %v", name, err, target, !want)
					}
				}

				// The message stays the one of the server.
				if strings.Count(err.Error(), tc.message) != 1 {
					t.Fatalf(
						"%s: error %q does not carry the message %q once",
						name, err, tc.message,
					)
				}
			}
		})
	}
}

func TestMissingConfig(t *testing.T) {
	t.Parallel()

	// Checked before the client connects, no server needed.
	c := &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	ctx := t.Context()

	for name, call := range map[string]func() error{
		"CreateContainer": func() error {
			_, err := c.CreateContainer(ctx, nil)

			return err
		},
		"ExecSyncContainer": func() error {
			_, err := c.ExecSyncContainer(ctx, nil)

			return err
		},
		"ReopenLogContainer": func() error { return c.ReopenLogContainer(ctx, nil) },
		"CreateNamespaces": func() error {
			_, err := c.CreateNamespaces(ctx, nil)

			return err
		},
		"ServeExecContainer": func() error {
			_, err := c.ServeExecContainer(ctx, nil)

			return err
		},
		"ServeAttachContainer": func() error {
			_, err := c.ServeAttachContainer(ctx, nil)

			return err
		},
		"ServePortForwardContainer": func() error {
			_, err := c.ServePortForwardContainer(ctx, nil)

			return err
		},
		"AttachContainer":        func() error { return c.AttachContainer(ctx, nil) },
		"SetWindowSizeContainer": func() error { return c.SetWindowSizeContainer(ctx, nil) },
	} {
		if err := call(); !errors.Is(err, ErrMissingConfig) {
			t.Errorf("%s: got error %v, want %v", name, err, ErrMissingConfig)
		}
	}

	// A nil port forward config always failed with the missing path.
	if _, err := c.ServePortForwardContainer(ctx, nil); !errors.Is(err, ErrMissingNetNsPath) {
		t.Errorf("got error %v, want %v", err, ErrMissingNetNsPath)
	}
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
	if err := c.KillContext(t.Context()); err != nil {
		t.Fatalf("kill again: %v", err)
	}

	// A zero PID would signal the process group of the caller.
	c = &ConmonClient{runDir: t.TempDir(), logger: logrus.New()}
	if err := c.KillContext(t.Context()); !errors.Is(err, errServerPIDUnknown) {
		t.Fatalf("got error %v, want %v", err, errServerPIDUnknown)
	}
}

func TestClientFieldLogger(t *testing.T) {
	t.Parallel()

	logger := logrus.New()
	entry := logrus.NewEntry(logrus.New()).WithField("component", "test")

	for _, tc := range []struct {
		name   string
		config ConmonServerConfig
		want   logrus.Ext1FieldLogger
	}{
		{"default", ConmonServerConfig{}, logrus.StandardLogger()},
		{"logger", ConmonServerConfig{ClientLogger: logger}, logger},
		{"entry", ConmonServerConfig{ClientFieldLogger: entry}, entry},
		{"entry first", ConmonServerConfig{ClientLogger: logger, ClientFieldLogger: entry}, entry},
		{"nil entry", ConmonServerConfig{ClientFieldLogger: (*logrus.Entry)(nil)}, logrus.StandardLogger()},
		{"nil entry and logger", ConmonServerConfig{ClientLogger: logger, ClientFieldLogger: (*logrus.Entry)(nil)}, logger},
	} {
		tc.config.ServerRunDir = t.TempDir()

		c, err := tc.config.toClient()
		if err != nil {
			t.Fatalf("%s: client: %v", tc.name, err)
		}

		if c.logger != tc.want {
			t.Errorf("%s: got logger %v, want %v", tc.name, c.logger, tc.want)
		}
	}
}

// recordingSpan records what endSpan does with it.
type recordingSpan struct {
	noop.Span

	err    error
	status codes.Code
	ended  bool
}

func (s *recordingSpan) RecordError(err error, _ ...trace.EventOption) {
	s.err = err
}

func (s *recordingSpan) SetStatus(code codes.Code, _ string) {
	s.status = code
}

func (s *recordingSpan) End(...trace.SpanEndOption) {
	s.ended = true
}

func TestEndSpan(t *testing.T) {
	t.Parallel()

	var err error

	// Without a tracer there is no span.
	endSpan(nil, &err)

	span := &recordingSpan{}
	endSpan(span, &err)

	if !span.ended || span.err != nil || span.status != codes.Unset {
		t.Fatalf("successful call recorded as %+v", span)
	}

	err = ErrUnsupported
	span = &recordingSpan{}
	endSpan(span, &err)

	if !span.ended || !errors.Is(span.err, ErrUnsupported) || span.status != codes.Error {
		t.Fatalf("failed call recorded as %+v", span)
	}
}

func TestVersionContext(t *testing.T) {
	t.Parallel()

	binary := writeTestBinary(t, `exec sleep 60`)

	ctx, cancel := context.WithTimeout(t.Context(), 100*time.Millisecond)
	defer cancel()

	start := time.Now()

	if _, err := VersionContext(ctx, binary); err == nil {
		t.Fatal("hanging binary succeeded")
	}

	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Fatalf("version took %s", elapsed)
	}
}

// newTestClient returns a client for the run directory, which is not
// connected to a server yet.
func newTestClient(t *testing.T, dir string, logger logrus.Ext1FieldLogger) *ConmonClient {
	t.Helper()

	c, err := (&ConmonServerConfig{ServerRunDir: dir}).toClient()
	if err != nil {
		t.Fatalf("client: %v", err)
	}

	c.logger = logger

	return c
}

// serveAttachSocket listens like the attach socket of a container and sends
// output on every connection until it gets closed.
func serveAttachSocket(t *testing.T) string {
	t.Helper()

	path := filepath.Join(t.TempDir(), "attach")

	listener, err := net.ListenUnix("unixpacket", &net.UnixAddr{Name: path, Net: "unixpacket"})
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	t.Cleanup(func() { listener.Close() })

	go func() {
		for {
			conn, err := listener.AcceptUnix()
			if err != nil {
				return
			}

			go func() {
				defer conn.Close()

				for {
					if _, err := conn.Write([]byte{attachPipeStdout, 'x'}); err != nil {
						return
					}

					time.Sleep(10 * time.Millisecond)
				}
			}()
		}
	}()

	return path
}

func TestCloseEndsAttach(t *testing.T) {
	t.Parallel()

	for _, withStdin := range []bool{false, true} {
		t.Run("stdin "+strconv.FormatBool(withStdin), func(t *testing.T) {
			t.Parallel()

			c := newTestClient(t, t.TempDir(), logrus.New())

			stdout, stdoutWriter := io.Pipe()
			cfg := &AttachConfig{
				ID:         "id",
				SocketPath: serveAttachSocket(t),
				Streams:    AttachStreams{Stdout: &Out{stdoutWriter}},
			}

			if withStdin {
				stdin, _ := io.Pipe()
				cfg.Streams.Stdin = &In{stdin}
			}

			attachErr := make(chan error, 1)

			go func() { attachErr <- c.attach(t.Context(), cfg) }()

			// The attach is running once the output arrives.
			if _, err := io.ReadFull(stdout, make([]byte, 1)); err != nil {
				t.Fatalf("read output: %v", err)
			}

			// The output stream gets closed.
			drained := make(chan error, 1)

			go func() {
				_, err := io.Copy(io.Discard, stdout)
				drained <- err
			}()

			if err := c.Close(); err != nil {
				t.Fatalf("close: %v", err)
			}

			select {
			case err := <-attachErr:
				if !errors.Is(err, ErrClientClosed) {
					t.Fatalf("got error %v, want %v", err, ErrClientClosed)
				}
			case <-time.After(10 * time.Second):
				t.Fatal("attach still running after Close")
			}

			if err := <-drained; err != nil {
				t.Fatalf("drain output: %v", err)
			}
		})
	}
}

func TestClosedFileStdinKeepsAttach(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	stdin, stdinWriter, err := os.Pipe()
	if err != nil {
		t.Fatalf("pipe: %v", err)
	}

	t.Cleanup(func() { stdinWriter.Close() })

	stdout, stdoutWriter := io.Pipe()
	cfg := &AttachConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: &In{stdin}, Stdout: &Out{stdoutWriter}},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	// The attach is running once the output arrives.
	if _, err := io.ReadFull(stdout, make([]byte, 1)); err != nil {
		t.Fatalf("read output: %v", err)
	}

	go func() { _, _ = io.Copy(io.Discard, stdout) }() //nolint:errcheck // drained only

	// Like on Shutdown, a read of the closed file fails with os.ErrClosed,
	// which does not end the attach.
	c.closeAttachStdins()

	select {
	case err := <-attachErr:
		t.Fatalf("attach ended with %v after its stdin got closed", err)
	case <-time.After(200 * time.Millisecond):
	}

	if err := c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if err := <-attachErr; !errors.Is(err, ErrClientClosed) {
		t.Fatalf("got error %v, want %v", err, ErrClientClosed)
	}
}

func TestDetachStopsOutput(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	stdin, stdinWriter := io.Pipe()
	// A slow stream, which is still being written to on the detach.
	stdout := &testOut{delay: 20 * time.Millisecond}
	cfg := &AttachConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: &In{stdin}, Stdout: &Out{stdout}},
		DetachKeys: []byte{16, 17},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	waitFor(t, "no output", func() bool {
		out, _ := stdout.state()

		return out != ""
	})

	// The keys get read one at a time, like from a terminal.
	for _, key := range cfg.DetachKeys {
		if _, err := stdinWriter.Write([]byte{key}); err != nil {
			t.Fatalf("write detach key: %v", err)
		}
	}

	if err := <-attachErr; !errors.Is(err, detach.ErrDetach) {
		t.Fatalf("got error %v, want %v", err, detach.ErrDetach)
	}

	// The output stopped before the attach returned.
	out, closed := stdout.state()
	if !closed {
		t.Fatal("stdout not closed when the attach returned")
	}

	time.Sleep(50 * time.Millisecond)

	if later, _ := stdout.state(); later != out {
		t.Fatalf("output written after the attach returned: %q", later[len(out):])
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

// cgroupManagerRequest records the cgroup manager of a request.
type cgroupManagerRequest struct {
	manager proto.Conmon_CgroupManager
}

func (r *cgroupManagerRequest) SetCgroupManager(manager proto.Conmon_CgroupManager) {
	r.manager = manager
}

func TestSetCgroupManagerWarnsOnce(t *testing.T) {
	t.Parallel()

	logger, hook := logrustest.NewNullLogger()

	c := newTestClient(t, t.TempDir(), logger)
	c.cgroupManager = CgroupManagerSystemd

	for range 3 {
		req := &cgroupManagerRequest{}
		c.setCgroupManager(CgroupManagerCgroupfs, req)

		if req.manager != proto.Conmon_CgroupManager_systemd {
			t.Fatalf("got cgroup manager %v, want the global one", req.manager)
		}
	}

	if got := len(hook.AllEntries()); got != 1 {
		t.Fatalf("got %d log entries, want one warning", got)
	}

	// The per command one gets used if the global one asks for it.
	c.cgroupManager = CgroupManagerPerCommand
	req := &cgroupManagerRequest{}
	c.setCgroupManager(CgroupManagerCgroupfs, req)

	if req.manager != proto.Conmon_CgroupManager_cgroupfs {
		t.Fatalf("got cgroup manager %v, want the per command one", req.manager)
	}
}
