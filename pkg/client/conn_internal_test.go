package client

import (
	"context"
	"errors"
	"strconv"
	"strings"
	"testing"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/exc"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
)

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

func TestClientReplacesUnusableConnection(t *testing.T) {
	t.Parallel()

	for _, withBootstrap := range []bool{true, false} {
		t.Run("bootstrap "+strconv.FormatBool(withBootstrap), func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			serveTestSocket(t, dir, withBootstrap)

			c := &ConmonClient{runDir: dir, logger: logrus.New()}
			t.Cleanup(c.resetConn)

			rpcClient, err := c.client(t.Context())
			if err != nil {
				t.Fatalf("create client: %v", err)
			}

			rpcClient.Release()

			first := c.conn

			if withBootstrap {
				if err = capnp.Client(first.client).Resolve(t.Context()); err != nil {
					t.Fatalf("resolve bootstrap: %v", err)
				}
			} else {
				waitBroken(t, first)
			}

			rpcClient, err = c.client(t.Context())
			if err != nil {
				t.Fatalf("create client again: %v", err)
			}

			rpcClient.Release()

			// A failed bootstrap makes the connection unusable for good.
			if replaced := c.conn != first; replaced == withBootstrap {
				t.Fatalf("connection replaced: %v, want %v", replaced, !withBootstrap)
			}

			if !withBootstrap {
				return
			}

			// A closed connection gets replaced as well.
			c.conn.conn.Close()

			rpcClient, err = c.client(t.Context())
			if err != nil {
				t.Fatalf("create client after close: %v", err)
			}

			rpcClient.Release()

			if c.conn == first {
				t.Fatal("closed connection not replaced")
			}
		})
	}
}

func TestDisconnectedCallsMatchErrServerDisconnected(t *testing.T) {
	t.Parallel()

	err := rpcError(exc.WrapError("call", exc.New(exc.Disconnected, "", "connection closed")))
	if !errors.Is(err, ErrServerDisconnected) {
		t.Fatalf("error %v does not match %v", err, ErrServerDisconnected)
	}

	// A failure the server reported is no disconnect.
	err = rpcError(exc.New(exc.Failed, "", "open log file: permission denied"))
	if errors.Is(err, ErrServerDisconnected) {
		t.Fatalf("error %v matches %v", err, ErrServerDisconnected)
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

func TestCallOnResetConnectionIsDisconnected(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	serveVersionSocket(t, dir, versionConmon{pid: 1})

	c := newTestClient(t, dir, logrus.New())

	t.Cleanup(func() { c.Close() })

	// A call which got the RPC client right before another one reset the
	// connection, like Close, Shutdown or a reconnect.
	rpcClient, err := c.client(t.Context())
	if err != nil {
		t.Fatalf("client: %v", err)
	}
	defer rpcClient.Release()

	c.resetConn()

	if _, err := c.version(t.Context(), rpcClient, nil); !errors.Is(err, ErrServerDisconnected) {
		t.Fatalf("got error %v, want %v", err, ErrServerDisconnected)
	}

	// The next call connects again.
	if _, err := c.Version(t.Context(), nil); err != nil {
		t.Fatalf("version after the reset: %v", err)
	}
}
