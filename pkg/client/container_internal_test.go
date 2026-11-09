package client

import (
	"context"
	"errors"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
)

func TestLogDriverTypes(t *testing.T) {
	t.Parallel()

	for typ, want := range map[LogDriverType]proto.Conmon_LogDriver_Type{
		LogDriverTypeContainerRuntimeInterface: proto.Conmon_LogDriver_Type_containerRuntimeInterface,
		LogDriverTypeJournald:                  proto.Conmon_LogDriver_Type_journald,
	} {
		got, err := logDriverTypes.toProto(typ)
		if err != nil || got != want {
			t.Errorf("type %d: got %v, %v, want %v", typ, got, err, want)
		}
	}

	logDrivers := []ContainerLogDriver{
		{Type: LogDriverTypeJournald, ContainerName: "name"},
		{Type: LogDriverTypeContainerRuntimeInterface, Path: "/log", MaxSize: 1},
	}

	types, err := validateLogDrivers(logDrivers)
	if err != nil {
		t.Fatalf("validate log drivers: %v", err)
	}

	// The request carries the fields of the drivers.
	_, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	req, err := proto.NewRootConmon_CreateContainerRequest(seg)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}

	err = initLogDrivers(&req, logDrivers, types)
	if err != nil {
		t.Fatalf("init log drivers: %v", err)
	}

	list, err := req.LogDrivers()
	if err != nil {
		t.Fatalf("log drivers: %v", err)
	}

	if list.Len() != 2 || list.At(0).Type() != proto.Conmon_LogDriver_Type_journald {
		t.Fatalf("got %d log drivers, the first of type %v", list.Len(), list.At(0).Type())
	}

	cri := list.At(1)

	path, err := cri.Path()
	if err != nil || cri.Type() != proto.Conmon_LogDriver_Type_containerRuntimeInterface ||
		path != "/log" || cri.MaxSize() != 1 {
		t.Errorf("got CRI driver %v, %q, %d, %v", cri.Type(), path, cri.MaxSize(), err)
	}

	if name, err := list.At(0).ContainerName(); err != nil || name != "name" {
		t.Errorf("got container name %q, %v, want %q", name, err, "name")
	}

	if _, err := validateLogDrivers([]ContainerLogDriver{
		{Type: LogDriverTypeJournald}, {Type: LogDriverType(42)},
	}); !errors.Is(err, ErrInvalidValue) {
		t.Errorf("unknown type: got %v, want %v", err, ErrInvalidValue)
	}
}

func TestCreateContainerValidatesWithoutServer(t *testing.T) {
	t.Parallel()

	// No server runs, the configuration fails before the client dials one.
	c := newTestClient(t, t.TempDir(), logrus.New())

	if _, err := c.CreateContainer(t.Context(), &CreateContainerConfig{
		ID:         "id",
		LogDrivers: []ContainerLogDriver{{Type: LogDriverType(42)}},
	}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("got error %v, want %v", err, ErrInvalidValue)
	}
}

// A restore on a server without the feature fails instead of creating a new
// container, a create does not need the feature.
func TestCreateContainerRestoreNeedsTheFeature(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	requests, _ := serveRecording(t, dir, FeatureIdleTimeout)

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	if _, err := c.CreateContainer(t.Context(), &CreateContainerConfig{
		ID:          "id",
		BundlePath:  "/bundle",
		RestorePath: "/checkpoint",
	}); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("got error %v, want %v", err, ErrUnsupported)
	}

	// Only the version got asked, the server got no create request.
	if got := lastRequest(t, requests); got.method != "Version" {
		t.Fatalf("got request %s %s, want Version", got.method, got.text)
	}

	if _, err := c.CreateContainer(t.Context(), &CreateContainerConfig{
		ID:         "id",
		BundlePath: "/bundle",
	}); err != nil {
		t.Fatalf("create: %v", err)
	}

	if got := lastRequest(t, requests); got.method != "CreateContainer" {
		t.Fatalf("got request %s %s, want CreateContainer", got.method, got.text)
	}
}

func TestExecTimeout(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name     string
		deadline time.Duration // none if 0
		timeout  time.Duration
		want     uint64
	}{
		{"explicit timeout below deadline", time.Hour, 5 * time.Second, 5},
		{"explicit timeout limited by deadline", 10500 * time.Millisecond, time.Hour, 9},
		{"explicit timeout a second below deadline", 10500 * time.Millisecond, 9 * time.Second, 9},
		{"explicit timeout at deadline", 10500 * time.Millisecond, 10 * time.Second, 9},
		{"explicit timeout without deadline", 0, 7 * time.Second, 7},
		{"explicit timeout rounded up", 0, 1500 * time.Millisecond, 2},
		{"short explicit timeout", 0, time.Millisecond, 1},
		{"negative timeout", 0, -time.Second, 0},
		{"no deadline", 0, 0, 0},
		{"deadline rounded down minus a second", 10500 * time.Millisecond, 0, 9},
		{"two seconds left", 2500 * time.Millisecond, 0, 1},
		{"less than two seconds left", 1900 * time.Millisecond, 0, 1},
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
