package client

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sync"
	"syscall"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
	logrustest "github.com/sirupsen/logrus/hooks/test"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
	"go.opentelemetry.io/otel/trace/noop"
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
		// The answers of other calls must not wait behind this one.
		call.Go()

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

// shortTimeouts returns the waits for servers which do not answer or go
// away, shortened for the tests which run into them on purpose.
func shortTimeouts() *serverTimeouts {
	return &serverTimeouts{
		probe:   500 * time.Millisecond,
		gone:    500 * time.Millisecond,
		cleanup: 500 * time.Millisecond,
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

	// Like a nil config, a missing terminal size fails before the client
	// connects.
	err := c.SetWindowSizeContainer(ctx, &SetWindowSizeContainerConfig{ID: "id"})
	if !errors.Is(err, ErrTerminalSizeNil) {
		t.Errorf("got error %v, want %v", err, ErrTerminalSizeNil)
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

func TestValidateCgroupManager(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	// A per command value which gets ignored is not checked.
	c.cgroupManager = CgroupManagerSystemd
	if err := c.validateCgroupManager(CgroupManager(42)); err != nil {
		t.Fatalf("ignored cgroup manager rejected: %v", err)
	}

	c.cgroupManager = CgroupManagerPerCommand

	for _, tc := range []struct {
		manager CgroupManager
		want    error
	}{
		{manager: CgroupManagerSystemd},
		{manager: CgroupManagerCgroupfs},
		{manager: CgroupManagerPerCommand, want: ErrCgroupManagerPerCommand},
		{manager: CgroupManager(42), want: ErrInvalidValue},
	} {
		if err := c.validateCgroupManager(tc.manager); !errors.Is(err, tc.want) {
			t.Fatalf("cgroup manager %d: got error %v, want %v", tc.manager, err, tc.want)
		}
	}

	// The global one gets checked by New.
	_, err := New(&ConmonServerConfig{ServerRunDir: t.TempDir(), CgroupManager: CgroupManager(42)})
	if !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("got error %v, want %v", err, ErrInvalidValue)
	}
}

func TestValidateLogLevelAndDriver(t *testing.T) {
	t.Parallel()

	for _, level := range []LogLevel{
		LogLevelTrace, LogLevelDebug, LogLevelInfo, LogLevelWarn, LogLevelError, LogLevelOff,
	} {
		if err := validateLogLevel(level); err != nil {
			t.Errorf("log level %q rejected: %v", level, err)
		}
	}

	for _, driver := range []LogDriver{LogDriverNone, LogDriverStdout, LogDriverSystemd} {
		if err := validateLogDriver(driver); err != nil {
			t.Errorf("log driver %q rejected: %v", driver, err)
		}
	}

	// The server only knows the lower case names.
	for _, err := range []error{
		validateLogLevel("Debug"),
		validateLogLevel(""),
		validateLogDriver("journald"),
		validateLogDriver(""),
	} {
		if !errors.Is(err, ErrInvalidValue) {
			t.Errorf("got error %v, want %v", err, ErrInvalidValue)
		}
	}
}

func TestTraceEnabled(t *testing.T) {
	t.Parallel()

	logger := logrus.New()
	logger.SetLevel(logrus.DebugLevel)

	for _, tc := range []struct {
		logger logrus.Ext1FieldLogger
		want   bool
	}{
		{logger: logger, want: false},
		{logger: logger.WithField("key", "value"), want: false},
	} {
		c := &ConmonClient{logger: tc.logger}
		if got := c.traceEnabled(); got != tc.want {
			t.Fatalf("%T: got %v, want %v", tc.logger, got, tc.want)
		}
	}

	logger.SetLevel(logrus.TraceLevel)

	c := &ConmonClient{logger: logger.WithField("key", "value")}
	if !c.traceEnabled() {
		t.Fatal("trace level not enabled")
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
