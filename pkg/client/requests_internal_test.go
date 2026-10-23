package client

import (
	"context"
	"fmt"
	"reflect"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
	"go.podman.io/common/pkg/resize"
)

// request is what a request of the client carried, as far as the tests of
// the requests look at it.
type request struct {
	method         string
	cgroupManager  proto.Conmon_CgroupManager
	additionalFDs  []uint64
	leakFDs        []uint64
	timeoutSec     uint64
	idleTimeoutSec uint64
	namespaces     []proto.Conmon_Namespace
	basePath       string
	podID          string
	netNsPath      string
	width, height  uint16
}

// recordingConmon answers the requests of the client with empty responses
// and records what they carried.
type recordingConmon struct {
	versionConmon

	requests chan<- request
}

func uint64s(list capnp.UInt64List, err error) []uint64 {
	if err != nil {
		return nil
	}

	if list.Len() == 0 {
		return nil
	}

	values := make([]uint64, list.Len())
	for i := range values {
		values[i] = list.At(i)
	}

	return values
}

func (r recordingConmon) CreateContainer(
	_ context.Context, call proto.Conmon_createContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{
		method:        "CreateContainer",
		cgroupManager: req.CgroupManager(),
		additionalFDs: uint64s(req.AdditionalFds()),
		leakFDs:       uint64s(req.LeakFds()),
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) ExecSyncContainer(
	_ context.Context, call proto.Conmon_execSyncContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{
		method:        "ExecSyncContainer",
		cgroupManager: req.CgroupManager(),
		timeoutSec:    req.TimeoutSec(),
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) CreateNamespaces(
	_ context.Context, call proto.Conmon_createNamespaces,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	recorded := request{method: "CreateNamespaces"}

	if list, listErr := req.Namespaces(); listErr == nil {
		for i := range list.Len() {
			recorded.namespaces = append(recorded.namespaces, list.At(i))
		}
	}

	recorded.basePath, _ = req.BasePath() //nolint:errcheck // empty if broken
	recorded.podID, _ = req.PodId()       //nolint:errcheck // empty if broken

	r.requests <- recorded

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) ServeExecContainer(
	_ context.Context, call proto.Conmon_serveExecContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{
		method:         "ServeExecContainer",
		cgroupManager:  req.CgroupManager(),
		idleTimeoutSec: req.IdleTimeoutSec(),
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) ServeAttachContainer(
	_ context.Context, call proto.Conmon_serveAttachContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{method: "ServeAttachContainer", idleTimeoutSec: req.IdleTimeoutSec()}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) ServePortForwardContainer(
	_ context.Context, call proto.Conmon_servePortForwardContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	netNsPath, _ := req.NetNsPath() //nolint:errcheck // empty if broken

	r.requests <- request{
		method:         "ServePortForwardContainer",
		idleTimeoutSec: req.IdleTimeoutSec(),
		netNsPath:      netNsPath,
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) SetWindowSizeContainer(
	_ context.Context, call proto.Conmon_setWindowSizeContainer,
) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{
		method: "SetWindowSizeContainer",
		width:  req.Width(),
		height: req.Height(),
	}

	results, err := call.AllocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func TestRequestsCarryTheConfig(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name string
		call func(context.Context, *ConmonClient) error
		want request
	}{
		{
			name: "CreateContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.CreateContainer(ctx, &CreateContainerConfig{
					ID:            "id",
					CgroupManager: CgroupManagerSystemd,
					AdditionalFDs: []RemoteFD{3, 4},
					LeakFDs:       []RemoteFD{5},
				})

				return err
			},
			want: request{
				method:        "CreateContainer",
				cgroupManager: proto.Conmon_CgroupManager_systemd,
				additionalFDs: []uint64{3, 4},
				leakFDs:       []uint64{5},
			},
		},
		{
			// The timeout of the context bounds the one of the exec.
			name: "ExecSyncContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				_, err := c.ExecSyncContainer(ctx, &ExecSyncConfig{
					ID:            "id",
					Timeout:       30,
					CgroupManager: CgroupManagerCgroupfs,
				})

				return err
			},
			want: request{
				method:        "ExecSyncContainer",
				cgroupManager: proto.Conmon_CgroupManager_cgroupfs,
				timeoutSec:    9,
			},
		},
		{
			// Without a base path the default one.
			name: "CreateNamespaces",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.CreateNamespaces(ctx, &CreateNamespacesConfig{
					Namespaces: []Namespace{NamespaceIPC, NamespaceNet},
					PodID:      "pod",
				})

				return err
			},
			want: request{
				method: "CreateNamespaces",
				namespaces: []proto.Conmon_Namespace{
					proto.Conmon_Namespace_ipc,
					proto.Conmon_Namespace_net,
				},
				basePath: "/var/run",
				podID:    "pod",
			},
		},
		{
			// Rounded up to whole seconds.
			name: "ServeExecContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeExecContainer(ctx, &ServeExecContainerConfig{
					ID:            "id",
					CgroupManager: CgroupManagerSystemd,
					IdleTimeout:   1500 * time.Millisecond,
				})

				return err
			},
			want: request{
				method:         "ServeExecContainer",
				cgroupManager:  proto.Conmon_CgroupManager_systemd,
				idleTimeoutSec: 2,
			},
		},
		{
			// A short timeout does not disable it.
			name: "ServeAttachContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeAttachContainer(ctx, &ServeAttachContainerConfig{
					ID:          "id",
					IdleTimeout: time.Nanosecond,
				})

				return err
			},
			want: request{method: "ServeAttachContainer", idleTimeoutSec: 1},
		},
		{
			name: "ServePortForwardContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServePortForwardContainer(ctx, &ServePortForwardContainerConfig{
					NetNsPath: "/proc/1/ns/net",
				})

				return err
			},
			want: request{method: "ServePortForwardContainer", netNsPath: "/proc/1/ns/net"},
		},
		{
			name: "SetWindowSizeContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				return c.SetWindowSizeContainer(ctx, &SetWindowSizeContainerConfig{
					ID:   "id",
					Size: &resize.TerminalSize{Width: 80, Height: 24},
				})
			},
			want: request{method: "SetWindowSizeContainer", width: 80, height: 24},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			requests := make(chan request, 1)
			server := recordingConmon{
				features: []string{FeaturePortForward},
				requests: requests,
			}

			dir := t.TempDir()
			serveSocket(t, dir, func() *rpc.Options {
				return &rpc.Options{
					BootstrapClient: capnp.Client(proto.Conmon_ServerToClient(server)),
				}
			})

			c := newTestClient(t, dir, logrus.New())
			c.cgroupManager = CgroupManagerPerCommand

			t.Cleanup(func() { c.Close() })

			if err := tc.call(t.Context(), c); err != nil {
				t.Fatalf("call: %v", err)
			}

			got := <-requests
			// Rounded down, up to a second may have passed until the
			// request got built.
			if tc.want.timeoutSec != 0 && got.timeoutSec+1 == tc.want.timeoutSec {
				got.timeoutSec = tc.want.timeoutSec
			}

			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got request %+v, want %+v", got, tc.want)
			}
		})
	}
}
