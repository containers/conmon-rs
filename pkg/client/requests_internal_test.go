package client

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"sync"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3/schemas"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
	"go.podman.io/common/pkg/resize"
	"go.podman.io/storage/pkg/idtools"
)

// registerSchemas registers the schema of the protocol, which the text
// encoding of the requests needs.
var registerSchemas = sync.OnceFunc(func() {
	proto.RegisterSchema(schemas.DefaultRegistry)
})

// request is a request of the client with its method and every field in
// the text encoding of Cap'n Proto.
type request struct {
	method string
	text   string
}

// recordingConmon answers the requests of the client with empty responses
// and records them.
type recordingConmon struct {
	versionConmon

	requests chan<- request
}

// respond records the request of a call and answers it with an empty
// response.
func respond[R interface{ NewResponse() (T, error) }, T any](
	r recordingConmon,
	method string,
	req fmt.Stringer,
	reqErr error,
	allocResults func() (R, error),
) error {
	if reqErr != nil {
		return fmt.Errorf("request: %w", reqErr)
	}

	r.requests <- request{method: method, text: req.String()}

	results, err := allocResults()
	if err != nil {
		return fmt.Errorf("alloc results: %w", err)
	}

	_, err = results.NewResponse()

	return err //nolint:wrapcheck // passed on to the client
}

func (r recordingConmon) Version(ctx context.Context, call proto.Conmon_version) error {
	req, err := call.Args().Request()
	if err != nil {
		return fmt.Errorf("request: %w", err)
	}

	r.requests <- request{method: "Version", text: req.String()}

	return r.versionConmon.Version(ctx, call)
}

func (r recordingConmon) CreateContainer(
	_ context.Context, call proto.Conmon_createContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "CreateContainer", req, err, call.AllocResults)
}

func (r recordingConmon) ExecSyncContainer(
	_ context.Context, call proto.Conmon_execSyncContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "ExecSyncContainer", req, err, call.AllocResults)
}

func (r recordingConmon) AttachContainer(
	_ context.Context, call proto.Conmon_attachContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "AttachContainer", req, err, call.AllocResults)
}

func (r recordingConmon) CreateNamespaces(
	_ context.Context, call proto.Conmon_createNamespaces,
) error {
	req, err := call.Args().Request()

	return respond(r, "CreateNamespaces", req, err, call.AllocResults)
}

func (r recordingConmon) ServeExecContainer(
	_ context.Context, call proto.Conmon_serveExecContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "ServeExecContainer", req, err, call.AllocResults)
}

func (r recordingConmon) ServeAttachContainer(
	_ context.Context, call proto.Conmon_serveAttachContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "ServeAttachContainer", req, err, call.AllocResults)
}

func (r recordingConmon) ServePortForwardContainer(
	_ context.Context, call proto.Conmon_servePortForwardContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "ServePortForwardContainer", req, err, call.AllocResults)
}

func (r recordingConmon) SetWindowSizeContainer(
	_ context.Context, call proto.Conmon_setWindowSizeContainer,
) error {
	req, err := call.Args().Request()

	return respond(r, "SetWindowSizeContainer", req, err, call.AllocResults)
}

// serveRecording serves a recordingConmon with the features on the socket
// of a client in dir, see serveSocket, and returns the channel with its
// requests.
func serveRecording(
	t *testing.T, dir string, features ...string,
) (requests <-chan request, stop func()) {
	t.Helper()

	registerSchemas()

	// Room for the Version call of a client which checks a feature first.
	recorded := make(chan request, 4)
	server := recordingConmon{
		features: features,
		requests: recorded,
	}

	return recorded, serveVersionSocket(t, dir, server)
}

// lastRequest returns the latest of the requests the server got so far.
func lastRequest(t *testing.T, requests <-chan request) request {
	t.Helper()

	var last request

	for {
		select {
		case req := <-requests:
			last = req
		default:
			if last.method == "" {
				t.Fatal("no request recorded")
			}

			return last
		}
	}
}

// attachWithoutSocket sends the attach request, the attach to the socket
// afterwards fails, there is none.
func attachWithoutSocket(ctx context.Context, c *ConmonClient, cfg *AttachConfig) error {
	if err := c.AttachContainer(ctx, cfg); err == nil {
		return errors.New("attached without a socket") //nolint:err113 // test only
	}

	return nil
}

// requestCase is a call of the client with the request it has to send.
type requestCase struct {
	name string
	call func(context.Context, *ConmonClient) error
	want request
}

// containerRequestCases returns the request cases of the version, container
// and window size calls.
func containerRequestCases() []requestCase {
	return []requestCase{
		{
			name: "Version",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.Version(ctx, &VersionConfig{Verbose: true})

				return err
			},
			want: request{
				method: "Version",
				text:   `(verbose = true, metadataOld = void, metadata = [])`,
			},
		},
		{
			// A nil config asks without the verbose fields.
			name: "Version without config",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.Version(ctx, nil)

				return err
			},
			want: request{
				method: "Version",
				text:   `(verbose = false, metadataOld = void, metadata = [])`,
			},
		},
		{
			name: "CreateContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.CreateContainer(ctx, &CreateContainerConfig{
					ID:           "id",
					BundlePath:   "/bundle",
					Terminal:     true,
					Stdin:        false,
					ExitPaths:    []string{"/exit-1", "/exit-2"},
					OOMExitPaths: []string{"/oom"},
					LogDrivers: []ContainerLogDriver{
						{
							Type:    LogDriverTypeContainerRuntimeInterface,
							Path:    "/cri.log",
							MaxSize: 1024,
						},
						{
							Type:          LogDriverTypeJournald,
							Path:          "/journald",
							MaxSize:       2048,
							ContainerName: "name",
							Tag:           "tag",
							Labels:        map[string]string{"LABEL": "label"},
						},
						{Type: LogDriverTypeJSONLogger, Path: "/json.log"},
					},
					CleanupCmd:     []string{"cleanup", "arg"},
					GlobalArgs:     []string{"--global"},
					CommandArgs:    []string{"--command"},
					EnvVars:        map[string]string{"ENV": "env"},
					CgroupManager:  CgroupManagerCgroupfs,
					AdditionalFDs:  []RemoteFD{3, 4},
					LeakFDs:        []RemoteFD{5},
					SdNotifySocket: "/notify.sock",
				})

				return err
			},
			want: request{
				method: "CreateContainer",
				text: `(id = "id", bundlePath = "/bundle", terminal = true, ` +
					`stdin = false, exitPaths = ["/exit-1", "/exit-2"], ` +
					`oomExitPaths = ["/oom"], logDrivers = [` +
					`(type = containerRuntimeInterface, path = "/cri.log", ` +
					`maxSize = 1024, containerName = "", tag = "", labels = []), ` +
					`(type = journald, path = "/journald", maxSize = 2048, ` +
					`containerName = "name", tag = "tag", ` +
					`labels = [(key = "LABEL", value = "label")]), ` +
					`(type = json, path = "/json.log", maxSize = 0, ` +
					`containerName = "", tag = "", labels = [])], ` +
					`cleanupCmd = ["cleanup", "arg"], globalArgs = ["--global"], ` +
					`commandArgs = ["--command"], metadataOld = void, ` +
					`metadata = [], envVars = [(key = "ENV", value = "env")], ` +
					`cgroupManager = cgroupfs, additionalFds = [3, 4], ` +
					`leakFds = [5], sdnotifySocket = "/notify.sock")`,
			},
		},
		{
			name: "CreateContainer with stdin",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.CreateContainer(ctx, &CreateContainerConfig{
					ID:    "id",
					Stdin: true,
				})

				return err
			},
			want: request{
				method: "CreateContainer",
				text: `(id = "id", bundlePath = "", terminal = false, ` +
					`stdin = true, exitPaths = [], oomExitPaths = [], ` +
					`logDrivers = [], cleanupCmd = [], globalArgs = [], ` +
					`commandArgs = [], metadataOld = void, metadata = [], ` +
					`envVars = [], cgroupManager = systemd, additionalFds = [], ` +
					`leakFds = [], sdnotifySocket = "")`,
			},
		},
		{
			name: "SetWindowSizeContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				return c.SetWindowSizeContainer(ctx, &SetWindowSizeContainerConfig{
					ID:   "id",
					Size: &resize.TerminalSize{Width: 80, Height: 24},
				})
			},
			want: request{
				method: "SetWindowSizeContainer",
				text: `(id = "id", width = 80, height = 24, ` +
					`metadataOld = void, metadata = [])`,
			},
		},
	}
}

// execRequestCases returns the request cases of the exec, attach and
// namespace calls.
func execRequestCases() []requestCase {
	return []requestCase{
		{
			// The deadline of the context bounds the timeout of the exec,
			// rounded down to at least a second, which stays 1 as long as the
			// request gets built within 0.99 seconds.
			name: "ExecSyncContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				ctx, cancel := context.WithTimeout(ctx, 1990*time.Millisecond)
				defer cancel()

				_, err := c.ExecSyncContainer(ctx, &ExecSyncConfig{
					ID:            "id",
					Command:       []string{"command", "arg"},
					Timeout:       30,
					Terminal:      true,
					EnvVars:       map[string]string{"ENV": "env"},
					CgroupManager: CgroupManagerCgroupfs,
				})

				return err
			},
			want: request{
				method: "ExecSyncContainer",
				text: `(id = "id", timeoutSec = 1, command = ["command", "arg"], ` +
					`terminal = true, metadataOld = void, metadata = [], ` +
					`envVars = [(key = "ENV", value = "env")], ` +
					`cgroupManager = cgroupfs)`,
			},
		},
		{
			name: "ExecSyncContainer without deadline",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ExecSyncContainer(ctx, &ExecSyncConfig{
					ID:      "id",
					Timeout: 30,
				})

				return err
			},
			want: request{
				method: "ExecSyncContainer",
				text: `(id = "id", timeoutSec = 30, command = [], ` +
					`terminal = false, metadataOld = void, metadata = [], ` +
					`envVars = [], cgroupManager = systemd)`,
			},
		},
		{
			name: "AttachContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				return attachWithoutSocket(ctx, c, &AttachConfig{
					ID:                "id",
					SocketPath:        "/attach.sock",
					ExecSession:       "exec",
					StopAfterStdinEOF: true,
				})
			},
			want: request{
				method: "AttachContainer",
				text: `(id = "id", socketPath = "/attach.sock", ` +
					`execSessionId = "exec", stopAfterStdinEof = true, ` +
					`metadataOld = void, metadata = [])`,
			},
		},
		{
			name: "AttachContainer without stdin once",
			call: func(ctx context.Context, c *ConmonClient) error {
				return attachWithoutSocket(ctx, c, &AttachConfig{
					ID:         "id",
					SocketPath: "/attach.sock",
				})
			},
			want: request{
				method: "AttachContainer",
				text: `(id = "id", socketPath = "/attach.sock", ` +
					`execSessionId = "", stopAfterStdinEof = false, ` +
					`metadataOld = void, metadata = [])`,
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
				text: `(metadataOld = void, namespaces = [ipc, net], ` +
					`uidMappings = [], gidMappings = [], basePath = "/var/run", ` +
					`podId = "pod", metadata = [])`,
			},
		},
		{
			name: "CreateNamespaces with user namespace",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.CreateNamespaces(ctx, &CreateNamespacesConfig{
					Namespaces: []Namespace{
						NamespacePID,
						NamespaceUser,
						NamespaceUTS,
					},
					IDMappings: idtools.NewIDMappingsFromMaps(
						[]idtools.IDMap{{ContainerID: 0, HostID: 1000, Size: 1}},
						[]idtools.IDMap{{ContainerID: 1, HostID: 2000, Size: 2}},
					),
					BasePath: "/base",
					PodID:    "pod",
				})

				return err
			},
			want: request{
				method: "CreateNamespaces",
				text: `(metadataOld = void, namespaces = [pid, user, uts], ` +
					`uidMappings = ["0 1000 1"], gidMappings = ["1 2000 2"], ` +
					`basePath = "/base", podId = "pod", metadata = [])`,
			},
		},
	}
}

// serveExecRequestCases returns the request cases of the streaming exec and
// port forward calls.
func serveExecRequestCases() []requestCase {
	return []requestCase{
		{
			// The idle timeout gets rounded up to whole seconds.
			name: "ServeExecContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeExecContainer(ctx, &ServeExecContainerConfig{
					ID:            "id",
					Command:       []string{"command", "arg"},
					Tty:           true,
					Stdin:         false,
					Stdout:        false,
					Stderr:        true,
					CgroupManager: CgroupManagerCgroupfs,
					IdleTimeout:   1500 * time.Millisecond,
				})

				return err
			},
			want: request{
				method: "ServeExecContainer",
				text: `(metadata = [], id = "id", command = ["command", "arg"], ` +
					`tty = true, stdin = false, stdout = false, stderr = true, ` +
					`cgroupManager = cgroupfs, idleTimeoutSec = 2)`,
			},
		},
		{
			name: "ServeExecContainer with stdin",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeExecContainer(ctx, &ServeExecContainerConfig{
					ID:     "id",
					Tty:    false,
					Stdin:  true,
					Stdout: false,
					Stderr: true,
				})

				return err
			},
			want: request{
				method: "ServeExecContainer",
				text: `(metadata = [], id = "id", command = [], tty = false, ` +
					`stdin = true, stdout = false, stderr = true, ` +
					`cgroupManager = systemd, idleTimeoutSec = 0)`,
			},
		},
		{
			name: "ServeExecContainer with stdout",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeExecContainer(ctx, &ServeExecContainerConfig{
					ID:     "id",
					Tty:    false,
					Stdin:  false,
					Stdout: true,
					Stderr: false,
				})

				return err
			},
			want: request{
				method: "ServeExecContainer",
				text: `(metadata = [], id = "id", command = [], tty = false, ` +
					`stdin = false, stdout = true, stderr = false, ` +
					`cgroupManager = systemd, idleTimeoutSec = 0)`,
			},
		},
		{
			name: "ServePortForwardContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServePortForwardContainer(ctx, &ServePortForwardContainerConfig{
					NetNsPath:   "/proc/1/ns/net",
					IdleTimeout: 3 * time.Second,
				})

				return err
			},
			want: request{
				method: "ServePortForwardContainer",
				text: `(metadata = [], netNsPath = "/proc/1/ns/net", ` +
					`idleTimeoutSec = 3)`,
			},
		},
	}
}

// serveAttachRequestCases returns the request cases of the streaming attach
// calls.
func serveAttachRequestCases() []requestCase {
	return []requestCase{
		{
			// A short idle timeout does not disable it.
			name: "ServeAttachContainer",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeAttachContainer(ctx, &ServeAttachContainerConfig{
					ID:                "id",
					Stdin:             true,
					Stdout:            false,
					Stderr:            false,
					StopAfterStdinEOF: true,
					IdleTimeout:       time.Nanosecond,
				})

				return err
			},
			want: request{
				method: "ServeAttachContainer",
				text: `(metadata = [], id = "id", stdin = true, stdout = false, ` +
					`stderr = false, stopAfterStdinEof = true, idleTimeoutSec = 1)`,
			},
		},
		{
			name: "ServeAttachContainer with stdout",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeAttachContainer(ctx, &ServeAttachContainerConfig{
					ID:                "id",
					Stdin:             false,
					Stdout:            true,
					Stderr:            false,
					StopAfterStdinEOF: true,
				})

				return err
			},
			want: request{
				method: "ServeAttachContainer",
				text: `(metadata = [], id = "id", stdin = false, stdout = true, ` +
					`stderr = false, stopAfterStdinEof = true, idleTimeoutSec = 0)`,
			},
		},
		{
			name: "ServeAttachContainer with stderr",
			call: func(ctx context.Context, c *ConmonClient) error {
				_, err := c.ServeAttachContainer(ctx, &ServeAttachContainerConfig{
					ID:                "id",
					Stdin:             false,
					Stdout:            false,
					Stderr:            true,
					StopAfterStdinEOF: false,
				})

				return err
			},
			want: request{
				method: "ServeAttachContainer",
				text: `(metadata = [], id = "id", stdin = false, stdout = false, ` +
					`stderr = true, stopAfterStdinEof = false, idleTimeoutSec = 0)`,
			},
		},
	}
}

// The values of the fields differ from each other and from the defaults,
// so that a setter which fills the wrong field fails the test. The booleans
// of a request are spread over several cases where needed, so that every
// pair of them differs once and each one gets set.
func TestRequestsCarryTheConfig(t *testing.T) {
	t.Parallel()

	for _, tc := range slices.Concat(
		containerRequestCases(),
		execRequestCases(),
		serveExecRequestCases(),
		serveAttachRequestCases(),
	) {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			requests, _ := serveRecording(t, dir, FeatureSdNotify, FeaturePortForward)

			c := newTestClient(t, dir, logrus.New())
			c.cgroupManager = CgroupManagerPerCommand

			t.Cleanup(func() { c.Close() })

			if err := tc.call(t.Context(), c); err != nil {
				t.Fatalf("call: %v", err)
			}

			if got := lastRequest(t, requests); got != tc.want {
				t.Fatalf("got request\n%s %s\nwant\n%s %s",
					got.method, got.text, tc.want.method, tc.want.text)
			}
		})
	}
}
