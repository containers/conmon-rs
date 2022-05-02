package client

import (
	"context"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
)

const (
	binaryName  = "conmonrs"
	socketName  = "conmon.sock"
	pidFileName = "pidfile"
)

type ConmonClient struct {
	serverPID uint32
	runDir    string
	logger    *logrus.Logger
}

// ConmonServerConfig is the configuration for the conmon server instance.
type ConmonServerConfig struct {
	// ClientLogger can be set to use a custom logger rather than the
	// logrus.StandardLogger.
	ClientLogger *logrus.Logger

	// ConmonServerPath is the binary path to the conmon server.
	ConmonServerPath string

	// LogLevel of the server to be used.
	// Can be "trace", "debug", "info", "warn", "error" or "off".
	LogLevel string

	// LogDriver is the possible server logging driver.
	// Can be "stdout" or "systemd".
	LogDriver string

	// Runtime is the binary path of the OCI runtime to use to operate on the
	// containers.
	Runtime string

	// RuntimeRoot is the root directory used by the OCI runtime to operate on
	// containers.
	RuntimeRoot string

	// ServerRunDir is the path of the directory for the server to hold files
	// at runtime.
	ServerRunDir string

	// Stdout is the standard output stream of the server when the log driver
	// "stdout" is being used (can be nil).
	Stdout io.WriteCloser

	// Stderr is the standard error stream of the server when the log driver
	// "stdout" is being used (can be nil).
	Stderr io.WriteCloser
}

// NewConmonServerConfig creates a new ConmonServerConfig instance for the
// required arguments. Optional arguments are pointing to their corresponding
// default values.
func NewConmonServerConfig(
	runtime, runtimeRoot, serverRunDir string,
) *ConmonServerConfig {
	return &ConmonServerConfig{
		LogLevel:     LogLevelDebug,
		LogDriver:    LogDriverStdout,
		Runtime:      runtime,
		RuntimeRoot:  runtimeRoot,
		ServerRunDir: serverRunDir,
		Stdout:       os.Stdout,
		Stderr:       os.Stderr,
	}
}

func New(config *ConmonServerConfig) (_ *ConmonClient, retErr error) {
	cl, err := config.ToClient()
	if err != nil {
		return nil, err
	}
	// Check if the process has already started, and inherit that process instead.
	if resp, err := cl.Version(context.Background()); err == nil {
		cl.serverPID = resp.ProcessID

		return cl, nil
	}
	if err := cl.StartServer(config); err != nil {
		return nil, err
	}

	pid, err := pidGivenFile(cl.pidFile())
	if err != nil {
		return nil, err
	}

	cl.serverPID = pid

	// Cleanup the background server process
	// if we fail any of the next steps
	defer func() {
		if retErr != nil {
			if err := cl.Shutdown(); err != nil {
				cl.logger.Errorf("Unable to shutdown server: %v", err)
			}
		}
	}()
	if err := cl.waitUntilServerUp(); err != nil {
		return nil, err
	}
	if err := os.Remove(cl.pidFile()); err != nil {
		return nil, err
	}

	return cl, nil
}

func (c *ConmonServerConfig) ToClient() (*ConmonClient, error) {
	if err := os.MkdirAll(c.ServerRunDir, 0o755); err != nil && !os.IsExist(err) {
		return nil, fmt.Errorf("couldn't create run dir %s", c.ServerRunDir)
	}

	if c.ClientLogger == nil {
		c.ClientLogger = logrus.StandardLogger()
	}

	return &ConmonClient{
		runDir: c.ServerRunDir,
		logger: c.ClientLogger,
	}, nil
}

func (c *ConmonClient) StartServer(config *ConmonServerConfig) error {
	entrypoint, args, err := c.toArgs(config)
	if err != nil {
		return err
	}
	cmd := exec.Command(entrypoint, args...)

	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setpgid: true,
	}

	if config.LogDriver == LogDriverStdout {
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
		if config.Stdout != nil {
			cmd.Stdout = config.Stdout
		}
		if config.Stderr != nil {
			cmd.Stderr = config.Stderr
		}
	}

	return cmd.Run()
}

func (c *ConmonClient) toArgs(config *ConmonServerConfig) (entrypoint string, args []string, err error) {
	const maxUnixSocketPathSize = len(syscall.RawSockaddrUnix{}.Path)
	if c == nil {
		return "", args, nil
	}
	entrypoint = config.ConmonServerPath
	if entrypoint == "" {
		path, err := exec.LookPath(binaryName)
		if err != nil {
			return "", args, fmt.Errorf("finding path: %w", err)
		}
		entrypoint = path
	}
	if config.Runtime == "" {
		return "", args, fmt.Errorf("runtime must be specified")
	}
	args = append(args, "--runtime", config.Runtime)

	if config.ServerRunDir == "" {
		return "", args, fmt.Errorf("RunDir must be specified")
	}
	args = append(args, "--runtime-dir", config.ServerRunDir)

	if config.RuntimeRoot != "" {
		args = append(args, "--runtime-root", config.RuntimeRoot)
	}

	if config.LogLevel != "" {
		if err := validateLogLevel(config.LogLevel); err != nil {
			return "", args, err
		}
		args = append(args, "--log-level", config.LogLevel)
	}

	if config.LogDriver != "" {
		if err := validateLogDriver(config.LogDriver); err != nil {
			return "", args, err
		}
		args = append(args, "--log-driver", config.LogDriver)
	}

	return entrypoint, args, nil
}

func validateLogLevel(level string) error {
	return validateStringSlice(
		"log level",
		level,
		LogLevelTrace, LogLevelDebug, LogLevelInfo, LogLevelWarn, LogLevelError, LogLevelOff,
	)
}

func validateLogDriver(driver string) error {
	return validateStringSlice(
		"log driver",
		driver,
		LogDriverStdout, LogDriverSystemd,
	)
}

func validateStringSlice(typ, given string, possibleValues ...string) error {
	for _, possibleValue := range possibleValues {
		if given == possibleValue {
			return nil
		}
	}

	return fmt.Errorf("%s %q is invalid", typ, given)
}

func pidGivenFile(file string) (uint32, error) {
	pidBytes, err := os.ReadFile(file)
	if err != nil {
		return 0, fmt.Errorf("reading pid bytes: %w", err)
	}
	pidU64, err := strconv.ParseUint(string(pidBytes), 10, 32)
	if err != nil {
		return 0, fmt.Errorf("parsing pid: %w", err)
	}

	return uint32(pidU64), nil
}

func (c *ConmonClient) waitUntilServerUp() (err error) {
	for i := 0; i < 100; i++ {
		_, err = c.Version(context.Background())
		if err == nil {
			break
		}
		time.Sleep(1 * time.Millisecond)
	}

	return err
}

func (c *ConmonClient) newRPCConn() (*rpc.Conn, error) {
	socketConn, err := DialLongSocket("unix", c.socket())
	if err != nil {
		return nil, err
	}

	return rpc.NewConn(rpc.NewStreamTransport(socketConn), nil), nil
}

// DialLongSocket is a wrapper around net.DialUnix.
// Its purpose is to allow for an arbitrarily long socket.
// It does so by opening the parent directory of path, and using the
// `/proc/self/fd` entry of that parent (which is a symlink to the actual parent)
// to construct the path to the socket.
// It assumes a valid path, as well as a file name that doesn't exceed the unix max socket length.
func DialLongSocket(network, path string) (*net.UnixConn, error) {
	parent := filepath.Dir(path)
	f, err := os.Open(parent)
	if err != nil {
		return nil, fmt.Errorf("open socket parent: %w", err)
	}
	defer f.Close()

	socketName := filepath.Base(path)

	socketPath := filepath.Join("/proc/self/fd", strconv.Itoa(int(f.Fd())), socketName)

	return net.DialUnix(network, nil, &net.UnixAddr{
		Name: socketPath, Net: network,
	})
}

// VersionResponse is the response of the Version method.
type VersionResponse struct {
	// Version is the actual version string of the server.
	Version string

	// Tag is the git tag of the server, empty if no tag is available.
	Tag string

	// Commit is git commit SHA of the build.
	Commit string

	// BuildDate is the date of build.
	BuildDate string

	// RustVersion is the used Rust version.
	RustVersion string

	// ProcessID is the PID of the server.
	ProcessID uint32
}

// Version can be used to retrieve all available version information.
func (c *ConmonClient) Version(ctx context.Context) (*VersionResponse, error) {
	conn, err := c.newRPCConn()
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.Version(ctx, nil)
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, err
	}

	response, err := result.Response()
	if err != nil {
		return nil, err
	}

	version, err := response.Version()
	if err != nil {
		return nil, err
	}

	tag, err := response.Tag()
	if err != nil {
		return nil, err
	}

	commit, err := response.Commit()
	if err != nil {
		return nil, err
	}

	buildDate, err := response.BuildDate()
	if err != nil {
		return nil, err
	}

	rustVersion, err := response.RustVersion()
	if err != nil {
		return nil, err
	}

	return &VersionResponse{
		Version:     version,
		Tag:         tag,
		Commit:      commit,
		BuildDate:   buildDate,
		RustVersion: rustVersion,
		ProcessID:   response.ProcessId(),
	}, nil
}

type CreateContainerConfig struct {
	ID         string
	BundlePath string
	Terminal   bool
	ExitPaths  []string
	LogDrivers []LogDriver
}

type LogDriver struct {
	Type LogDriverType
	Path string
}

type LogDriverType int

const (
	LogDriverTypeContainerRuntimeInterface LogDriverType = iota
)

type CreateContainerResponse struct {
	PID uint32
}

func (c *ConmonClient) CreateContainer(
	ctx context.Context, cfg *CreateContainerConfig,
) (*CreateContainerResponse, error) {
	conn, err := c.newRPCConn()
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.CreateContainer(ctx, func(p proto.Conmon_createContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return err
		}
		if err := req.SetId(cfg.ID); err != nil {
			return err
		}
		if err := req.SetBundlePath(cfg.BundlePath); err != nil {
			return err
		}
		req.SetTerminal(cfg.Terminal)
		if err := stringSliceToTextList(cfg.ExitPaths, req.NewExitPaths); err != nil {
			return err
		}

		if err := c.initLogDrivers(&req, cfg.LogDrivers); err != nil {
			return err
		}

		return p.SetRequest(req)
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, err
	}

	response, err := result.Response()
	if err != nil {
		return nil, err
	}

	return &CreateContainerResponse{
		PID: response.ContainerPid(),
	}, nil
}

type ExecSyncConfig struct {
	ID       string
	Command  []string
	Timeout  uint64
	Terminal bool
}

type ExecContainerResult struct {
	ExitCode int32
	Stdout   []byte
	Stderr   []byte
	TimedOut bool
}

func (c *ConmonClient) ExecSyncContainer(ctx context.Context, cfg *ExecSyncConfig) (*ExecContainerResult, error) {
	conn, err := c.newRPCConn()
	if err != nil {
		return nil, err
	}
	defer conn.Close()

	client := proto.Conmon{Client: conn.Bootstrap(ctx)}
	future, free := client.ExecSyncContainer(ctx, func(p proto.Conmon_execSyncContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return err
		}
		if err := req.SetId(cfg.ID); err != nil {
			return err
		}
		req.SetTimeoutSec(cfg.Timeout)
		if err := stringSliceToTextList(cfg.Command, req.NewCommand); err != nil {
			return err
		}
		req.SetTerminal(cfg.Terminal)
		if err := p.SetRequest(req); err != nil {
			return err
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, err
	}

	resp, err := result.Response()
	if err != nil {
		return nil, err
	}

	stdout, err := resp.Stdout()
	if err != nil {
		return nil, err
	}

	stderr, err := resp.Stderr()
	if err != nil {
		return nil, err
	}

	execContainerResult := &ExecContainerResult{
		ExitCode: resp.ExitCode(),
		Stdout:   stdout,
		Stderr:   stderr,
		TimedOut: resp.TimedOut(),
	}

	return execContainerResult, nil
}

func stringSliceToTextList(src []string, newFunc func(int32) (capnp.TextList, error)) error {
	l := int32(len(src))
	if l == 0 {
		return nil
	}
	list, err := newFunc(l)
	if err != nil {
		return err
	}
	for i := 0; i < len(src); i++ {
		if err := list.Set(i, src[i]); err != nil {
			return err
		}
	}

	return nil
}

func (c *ConmonClient) initLogDrivers(req *proto.Conmon_CreateContainerRequest, logDrivers []LogDriver) error {
	newLogDrivers, err := req.NewLogDrivers(int32(len(logDrivers)))
	if err != nil {
		return err
	}
	for i, logDriver := range logDrivers {
		n := newLogDrivers.At(i)
		if logDriver.Type == LogDriverTypeContainerRuntimeInterface {
			n.SetType(proto.Conmon_LogDriver_Type_containerRuntimeInterface)
		}
		if err := n.SetPath(logDriver.Path); err != nil {
			return err
		}
	}

	return nil
}

// TODO FIXME test only?
func (c *ConmonClient) PID() uint32 {
	return c.serverPID
}

func (c *ConmonClient) Shutdown() error {
	return syscall.Kill(int(c.serverPID), syscall.SIGINT)
}

func (c *ConmonClient) pidFile() string {
	return filepath.Join(c.runDir, pidFileName)
}

func (c *ConmonClient) socket() string {
	return filepath.Join(c.runDir, socketName)
}

type ReopenLogContainerConfig struct {
	ID string
}

func (c *ConmonClient) ReopenLogContainer(ctx context.Context, cfg *ReopenLogContainerConfig) error {
	conn, err := c.newRPCConn()
	if err != nil {
		return err
	}
	defer conn.Close()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.ReopenLogContainer(ctx, func(p proto.Conmon_reopenLogContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return err
		}
		if err := req.SetId(cfg.ID); err != nil {
			return err
		}

		return p.SetRequest(req)
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return err
	}

	if _, err := result.Response(); err != nil {
		return err
	}

	return nil
}
