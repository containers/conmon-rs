package client

import (
	"context"
	"errors"
	"fmt"
	"os"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
)

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
