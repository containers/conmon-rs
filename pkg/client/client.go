package client

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"sync"
	"syscall"
	"time"

	"capnproto.org/go/capnp/v3/rpc"
	"github.com/blang/semver/v4"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/containers/storage/pkg/idtools"
	"github.com/sirupsen/logrus"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/propagation"
	"go.opentelemetry.io/otel/trace"
)

const (
	binaryName     = "conmonrs"
	socketName     = "conmon.sock"
	pidFileName    = "pidfile"
	defaultTimeout = 10 * time.Second
)

var (
	errRuntimeUnspecified     = errors.New("runtime must be specified")
	errRunDirUnspecified      = errors.New("RunDir must be specified")
	errInvalidValue           = errors.New("invalid value")
	errRunDirNotCreated       = errors.New("could not create RunDir")
	errTimeoutWaitForPid      = errors.New("timed out waiting for server PID to disappear")
	errUndefinedCgroupManager = errors.New("undefined cgroup manager")
)

// ConmonClient is the main client structure of this package.
type ConmonClient struct {
	serverPID      uint32
	runDir         string
	logger         *logrus.Logger
	attachReaders  *sync.Map // K: UUID string, V: *attachReaderValue
	tracingEnabled bool
	tracer         trace.Tracer
	serverVersion  semver.Version
	cgroupManager  CgroupManager
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
	LogLevel LogLevel

	// LogDriver is the possible server logging driver.
	// Can be "stdout" or "systemd".
	LogDriver LogDriver

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

	// CgroupManager can be use to select the cgroup manager.
	CgroupManager CgroupManager

	// Tracing can be used to enable OpenTelemetry tracing.
	Tracing *Tracing
}

// Tracing is the structure for managing server-side OpenTelemetry tracing.
type Tracing struct {
	// Enabled tells the server to run with OpenTelemetry tracing.
	Enabled bool

	// Endpoint is the GRPC tracing endpoint for OLTP.
	// Defaults to "http://127.0.0.1:4317"
	Endpoint string

	// Tracer allows the client to create additional spans if set.
	Tracer trace.Tracer
}

// NewConmonServerConfig creates a new ConmonServerConfig instance for the
// required arguments. Optional arguments are pointing to their corresponding
// default values.
func NewConmonServerConfig(
	runtime, runtimeRoot, serverRunDir string,
) *ConmonServerConfig {
	return &ConmonServerConfig{
		LogLevel:     LogLevelDebug,
		LogDriver:    LogDriverSystemd,
		Runtime:      runtime,
		RuntimeRoot:  runtimeRoot,
		ServerRunDir: serverRunDir,
		Stdout:       os.Stdout,
		Stderr:       os.Stderr,
	}
}

// FromLogrusLevel converts the logrus.Level to a conmon-rs server log level.
func FromLogrusLevel(level logrus.Level) LogLevel {
	switch level {
	case logrus.PanicLevel, logrus.FatalLevel:
		return LogLevelOff

	case logrus.ErrorLevel:
		return LogLevelError

	case logrus.WarnLevel:
		return LogLevelWarn

	case logrus.InfoLevel:
		return LogLevelInfo

	case logrus.DebugLevel:
		return LogLevelDebug

	case logrus.TraceLevel:
		return LogLevelTrace
	}

	return LogLevelDebug
}

// New creates a new conmon server, starts it and connects a new client to it.
//
// If a server is already started with the same `ServerRunDir` specified
// the client connects to the existing server instead.
// Note: Other options from the `ConmonServerConfig` will be ignored
// and the settings of the existing server will remain unchanged.
func New(config *ConmonServerConfig) (client *ConmonClient, retErr error) {
	cl, err := config.toClient()
	if err != nil {
		return nil, fmt.Errorf("convert config to client: %w", err)
	}
	// Check if the process has already started, and inherit that process instead.
	ctx, cancel := defaultContext()
	defer cancel()

	ctx, span := cl.startSpan(ctx, "New")
	if span != nil {
		defer span.End()
	}

	if resp, err := cl.Version(ctx, &VersionConfig{}); err == nil {
		cl.serverPID = resp.ProcessID

		return cl, nil
	}

	if err := cl.startServer(config); err != nil {
		return nil, fmt.Errorf("start server: %w", err)
	}

	pid, err := pidGivenFile(cl.pidFile())
	if err != nil {
		return nil, fmt.Errorf("get pid from env: %w", err)
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
		return nil, fmt.Errorf("wait until server is up: %w", err)
	}

	if err := os.Remove(cl.pidFile()); err != nil {
		return nil, fmt.Errorf("remove pid file: %w", err)
	}

	return cl, nil
}

// ServerVersion contains the version data of the server instance.
type ServerVersion struct {
	// The current version.
	Version string `json:"version"`

	// The tag of the build, empty if not available.
	Tag string `json:"tag"`

	// The git commit SHA of the build.
	Commit string `json:"commit"`

	// The build date string.
	BuildDate string `json:"build_date"` //nolint:tagliatelle // Rust's serde will use that format.

	// The target triple string.
	Target string `json:"target"`

	// The used Rust version.
	RustVersion string `json:"rust_version"` //nolint:tagliatelle // Rust's serde will use that format.

	// The used Cargo version.
	CargoVersion string `json:"cargo_version"` //nolint:tagliatelle // Rust's serde will use that format.

	// The cargo dependency tree, only available in verbose output.
	CargoTree string `json:"cargo_tree"` //nolint:tagliatelle // Rust's serde will use that format.
}

// Version can be used to retrieve the server version without requiring a
// running Server.
//
// If binaryPath is empty, then the default binary will be used.
// If the server doesn't support it, then an ErrUnsupported error is returned.
func Version(binaryPath string) (res *ServerVersion, err error) {
	if binaryPath == "" {
		binaryPath = binaryName
	}

	const arg = "--version-json"

	data, err := exec.Command(binaryPath, arg).CombinedOutput()
	if err != nil {
		if bytes.Contains(data, []byte("error: unexpected argument")) {
			return nil, ErrUnsupported
		}

		return nil, fmt.Errorf("run `%s %s`: %s: %w", binaryPath, arg, string(data), err)
	}

	if err := json.Unmarshal(data, &res); err != nil {
		return nil, fmt.Errorf("unmarshal result: %w", err)
	}

	return res, nil
}

func (c *ConmonServerConfig) toClient() (*ConmonClient, error) {
	const perm = 0o755
	if err := os.MkdirAll(c.ServerRunDir, perm); err != nil && !os.IsExist(err) {
		return nil, fmt.Errorf("%s: %w", c.ServerRunDir, errRunDirNotCreated)
	}

	if c.ClientLogger == nil {
		c.ClientLogger = logrus.StandardLogger()
	}

	var tracer trace.Tracer
	if c.Tracing != nil && c.Tracing.Tracer != nil {
		tracer = c.Tracing.Tracer
	}

	return &ConmonClient{
		runDir:        c.ServerRunDir,
		logger:        c.ClientLogger,
		attachReaders: &sync.Map{},
		tracer:        tracer,
		cgroupManager: c.CgroupManager,
	}, nil
}

//nolint:ireturn,nolintlint // Returning the interface is intentional
func (c *ConmonClient) startSpan(ctx context.Context, name string) (context.Context, trace.Span) {
	if c.tracer == nil {
		return ctx, nil
	}

	const prefix = "conmonrs-client: "

	//nolint:spancheck // https://github.com/jjti/go-spancheck/issues/7
	return c.tracer.Start(ctx, prefix+name, trace.WithSpanKind(trace.SpanKindClient))
}

func (c *ConmonClient) startServer(config *ConmonServerConfig) error {
	_, span := c.startSpan(context.TODO(), "startServer")
	if span != nil {
		defer span.End()
	}

	entrypoint, args, err := c.toArgs(config)
	if err != nil {
		return fmt.Errorf("convert config to args: %w", err)
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

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("run server command: %w", err)
	}

	return nil
}

func (c *ConmonClient) toArgs(config *ConmonServerConfig) (entrypoint string, args []string, err error) {
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
		return "", args, errRuntimeUnspecified
	}

	args = append(args, "--runtime", config.Runtime)

	if config.ServerRunDir == "" {
		return "", args, errRunDirUnspecified
	}

	args = append(args, "--runtime-dir", config.ServerRunDir)

	if config.RuntimeRoot != "" {
		args = append(args, "--runtime-root", config.RuntimeRoot)
	}

	if config.LogLevel != "" {
		if err := validateLogLevel(config.LogLevel); err != nil {
			return "", args, fmt.Errorf("validate log level: %w", err)
		}

		args = append(args, "--log-level", string(config.LogLevel))
	}

	if config.LogDriver != "" {
		if err := validateLogDriver(config.LogDriver); err != nil {
			return "", args, fmt.Errorf("validate log driver: %w", err)
		}

		args = append(args, "--log-driver", string(config.LogDriver))
	}

	const cgroupManagerFlag = "--cgroup-manager"

	switch config.CgroupManager {
	case CgroupManagerSystemd:
		args = append(args, cgroupManagerFlag, "systemd")

	case CgroupManagerCgroupfs:
		args = append(args, cgroupManagerFlag, "cgroupfs")

	case CgroupManagerPerCommand:
		// nothing to do, will use the cgroup manager specified per command

	default:
		return "", args, errUndefinedCgroupManager
	}

	if config.Tracing != nil && config.Tracing.Enabled {
		c.tracingEnabled = true

		args = append(args, "--enable-tracing")

		if config.Tracing.Endpoint != "" {
			args = append(args, "--tracing-endpoint", config.Tracing.Endpoint)
		}
	}

	return entrypoint, args, nil
}

func validateLogLevel(level LogLevel) error {
	return validateStringSlice(
		"log level",
		string(level),
		string(LogLevelTrace),
		string(LogLevelDebug),
		string(LogLevelInfo),
		string(LogLevelWarn),
		string(LogLevelError),
		string(LogLevelOff),
	)
}

func validateLogDriver(driver LogDriver) error {
	return validateStringSlice(
		"log driver",
		string(driver),
		string(LogDriverStdout),
		string(LogDriverSystemd),
	)
}

func validateStringSlice(typ, given string, possibleValues ...string) error {
	for _, possibleValue := range possibleValues {
		if given == possibleValue {
			return nil
		}
	}

	return fmt.Errorf("%w: %s %q", errInvalidValue, typ, given)
}

func pidGivenFile(file string) (uint32, error) {
	pidBytes, err := os.ReadFile(file)
	if err != nil {
		return 0, fmt.Errorf("reading pid bytes: %w", err)
	}

	const (
		base    = 10
		bitSize = 32
	)

	pidU64, err := strconv.ParseUint(string(pidBytes), base, bitSize)
	if err != nil {
		return 0, fmt.Errorf("parsing pid: %w", err)
	}

	return uint32(pidU64), nil
}

func (c *ConmonClient) waitUntilServerUp() (err error) {
	_, span := c.startSpan(context.TODO(), "waitUntilServerUp")
	if span != nil {
		defer span.End()
	}

	for range 100 {
		ctx, cancel := defaultContext()

		_, err = c.Version(ctx, &VersionConfig{})
		if err == nil {
			cancel()

			break
		}

		cancel()
		time.Sleep(1 * time.Millisecond)
	}

	return err
}

func defaultContext() (context.Context, context.CancelFunc) {
	return context.WithTimeout(context.Background(), defaultTimeout)
}

func (c *ConmonClient) newRPCConn() (*rpc.Conn, error) {
	socketConn, err := DialLongSocket("unix", c.socket())
	if err != nil {
		return nil, fmt.Errorf("dial long socket: %w", err)
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

	const procSelfFDPath = "/proc/self/fd"

	socketPath := filepath.Join(procSelfFDPath, strconv.Itoa(int(f.Fd())), socketName)

	conn, err := net.DialUnix(network, nil, &net.UnixAddr{
		Name: socketPath, Net: network,
	})
	if err != nil {
		return nil, fmt.Errorf("dial unix socket: %w", err)
	}

	return conn, nil
}

// VersionConfig is the configuration for calling the Version method.
type VersionConfig struct {
	// Verbose specifies verbose version output.
	Verbose bool
}

// VersionResponse is the response of the Version method.
type VersionResponse struct {
	// ProcessID is the PID of the server.
	ProcessID uint32

	// Version is the actual version string of the server.
	Version string

	// Tag is the git tag of the server, empty if no tag is available.
	Tag string

	// Commit is git commit SHA of the build.
	Commit string

	// BuildDate is the date of build.
	BuildDate string

	// Target is the build triple.
	Target string

	// RustVersion is the used Rust version.
	RustVersion string

	// CargoVersion is the used Cargo version.
	CargoVersion string

	// CargoTree is the used dependency tree.
	// Only set if request was in verbose mode.
	CargoTree string
}

// Version can be used to retrieve all available version information.
func (c *ConmonClient) Version(
	ctx context.Context, cfg *VersionConfig,
) (*VersionResponse, error) {
	ctx, span := c.startSpan(ctx, "Version")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}

	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.Version(ctx, func(p proto.Conmon_version_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		verbose := false
		if cfg != nil {
			verbose = cfg.Verbose
		}

		req.SetVerbose(verbose)

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	response, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	version, err := response.Version()
	if err != nil {
		return nil, fmt.Errorf("set version: %w", err)
	}

	semverVersion, err := semver.Parse(version)
	if err != nil {
		return nil, fmt.Errorf("parse server version to semver: %w", err)
	}

	c.serverVersion = semverVersion

	tag, err := response.Tag()
	if err != nil {
		return nil, fmt.Errorf("set tag: %w", err)
	}

	commit, err := response.Commit()
	if err != nil {
		return nil, fmt.Errorf("set commit: %w", err)
	}

	buildDate, err := response.BuildDate()
	if err != nil {
		return nil, fmt.Errorf("set build date: %w", err)
	}

	target, err := response.Target()
	if err != nil {
		return nil, fmt.Errorf("set target: %w", err)
	}

	rustVersion, err := response.RustVersion()
	if err != nil {
		return nil, fmt.Errorf("set rust version: %w", err)
	}

	cargoVersion, err := response.CargoVersion()
	if err != nil {
		return nil, fmt.Errorf("set cargo version: %w", err)
	}

	cargoTree, err := response.CargoTree()
	if err != nil {
		return nil, fmt.Errorf("set cargo version: %w", err)
	}

	return &VersionResponse{
		ProcessID:    response.ProcessId(),
		Version:      version,
		Tag:          tag,
		Commit:       commit,
		BuildDate:    buildDate,
		Target:       target,
		RustVersion:  rustVersion,
		CargoVersion: cargoVersion,
		CargoTree:    cargoTree,
	}, nil
}

func (c *ConmonClient) setCgroupManager(
	cfg CgroupManager,
	req interface {
		SetCgroupManager(manager proto.Conmon_CgroupManager)
	},
) {
	cgroupManager := c.cgroupManager
	if cgroupManager == CgroupManagerPerCommand {
		cgroupManager = cfg
	} else if cfg != CgroupManager(0) {
		c.logger.Warnf(
			"cgroup manager specified in per command config, but global cgroup manager is not set to CgroupManagerPerCommand",
		)
	}

	req.SetCgroupManager(proto.Conmon_CgroupManager(cgroupManager))
}

// CreateContainerConfig is the configuration for calling the CreateContainer
// method.
type CreateContainerConfig struct {
	// ID is the container identifier.
	ID string

	// BundlePath is the path to the filesystem bundle.
	BundlePath string

	// Terminal indicates if a tty should be used or not.
	Terminal bool

	// Stdin indicates if stdin should be available or not.
	Stdin bool

	// ExitPaths is a slice of paths to write the exit statuses.
	ExitPaths []string

	// OOMExitPaths is a slice of files that should be created if the given container is OOM killed.
	OOMExitPaths []string

	// LogDrivers is a slice of selected log drivers.
	LogDrivers []ContainerLogDriver

	// CleanupCmd is the command that will be executed once the container exits
	CleanupCmd []string

	// GlobalArgs are the additional arguments passed to the create runtime call
	// before the command. e.g: crun --runtime-arg create
	GlobalArgs []string

	// CommandArgs are the additional arguments passed to the create runtime call
	// after the command. e.g: crun create --runtime-opt
	CommandArgs []string

	// EnvVars are the environment variables passed to the create runtime call.
	EnvVars map[string]string

	// CgroupManager can be use to select the cgroup manager.
	//
	// To use this option set `ConmonServerConfig.CgroupManager` to `CgroupManagerPerCommand`.
	CgroupManager CgroupManager

	// AdditionalFDs can be used to pass additional file descriptors to the container.
	AdditionalFDs []RemoteFD

	// LeakFDs can be used to keep file descriptors open as long as the container is running.
	LeakFDs []RemoteFD
}

// ContainerLogDriver specifies a selected logging mechanism.
type ContainerLogDriver struct {
	// Type defines the log driver variant.
	Type LogDriverType

	// Path specifies the filesystem path of the log driver.
	Path string

	// MaxSize is the maximum amount of bytes to be written before rotation.
	// 0 translates to an unlimited size.
	MaxSize uint64
}

// LogDriverType specifies available log drivers.
type LogDriverType int

const (
	// LogDriverTypeContainerRuntimeInterface is the Kubernetes CRI logger
	// type.
	LogDriverTypeContainerRuntimeInterface LogDriverType = iota
	LogDriverTypeJSONLogger                LogDriverType = iota
	LogDriverTypeJournald                  LogDriverType = iota
)

// CreateContainerResponse is the response of the CreateContainer method.
type CreateContainerResponse struct {
	// PID is the container process identifier.
	PID uint32

	// NamespacesPath is the base path where the namespaces are mounted.
	NamespacesPath string
}

// CreateContainer can be used to create a new running container instance.
func (c *ConmonClient) CreateContainer(
	ctx context.Context, cfg *CreateContainerConfig,
) (*CreateContainerResponse, error) {
	ctx, span := c.startSpan(ctx, "CreateContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}

	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.CreateContainer(ctx, func(p proto.Conmon_createContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetId(cfg.ID); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		if err := req.SetBundlePath(cfg.BundlePath); err != nil {
			return fmt.Errorf("set bundle path: %w", err)
		}

		req.SetTerminal(cfg.Terminal)
		req.SetStdin(cfg.Stdin)

		if err := stringSliceToTextList(cfg.ExitPaths, req.NewExitPaths); err != nil {
			return fmt.Errorf("convert exit paths string slice to text list: %w", err)
		}

		if err := stringSliceToTextList(cfg.OOMExitPaths, req.NewOomExitPaths); err != nil {
			return fmt.Errorf("convert oom exit paths string slice to text list: %w", err)
		}

		if err := c.initLogDrivers(&req, cfg.LogDrivers); err != nil {
			return fmt.Errorf("init log drivers: %w", err)
		}

		if err := stringSliceToTextList(cfg.CleanupCmd, req.NewCleanupCmd); err != nil {
			return fmt.Errorf("convert cleanup command string slice to text list: %w", err)
		}

		if err := stringSliceToTextList(cfg.GlobalArgs, req.NewGlobalArgs); err != nil {
			return fmt.Errorf("convert global arguments string slice to text list: %w", err)
		}

		if err := stringSliceToTextList(cfg.CommandArgs, req.NewCommandArgs); err != nil {
			return fmt.Errorf("convert command arguments string slice to text list: %w", err)
		}

		if err := stringStringMapToMapEntryList(cfg.EnvVars, req.NewEnvVars); err != nil {
			return fmt.Errorf("convert environment variables string slice to text list: %w", err)
		}

		c.setCgroupManager(cfg.CgroupManager, req)

		if err := remoteFDSliceToUInt64List(cfg.AdditionalFDs, req.NewAdditionalFds); err != nil {
			return fmt.Errorf("convert file descriptor slice to slot list: %w", err)
		}

		if err := remoteFDSliceToUInt64List(cfg.LeakFDs, req.NewLeakFds); err != nil {
			return fmt.Errorf("convert file descriptor slice to slot list: %w", err)
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	response, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	return &CreateContainerResponse{
		PID: response.ContainerPid(),
	}, nil
}

// ExecSyncConfig is the configuration for calling the ExecSyncContainer
// method.
type ExecSyncConfig struct {
	// ID is the container identifier.
	ID string

	// Command is a slice of command line arguments.
	Command []string

	// Timeout is the maximum time the command can run in seconds.
	Timeout uint64

	// Terminal specifies if a tty should be used.
	Terminal bool

	// EnvVars are the environment variables passed to the exec runtime call.
	EnvVars map[string]string

	// CgroupManager can be use to select the cgroup manager.
	//
	// To use this option set `ConmonServerConfig.CgroupManager` to `CgroupManagerPerCommand`.
	CgroupManager CgroupManager
}

// ExecContainerResult is the result for calling the ExecSyncContainer method.
type ExecContainerResult struct {
	// ExitCode specifies the returned exit status.
	ExitCode int32

	// Stdout contains the stdout stream result.
	Stdout []byte

	// Stderr contains the stderr stream result.
	Stderr []byte

	// TimedOut is true if the command timed out.
	TimedOut bool
}

// ExecSyncContainer can be used to execute a command within a running
// container.
func (c *ConmonClient) ExecSyncContainer(ctx context.Context, cfg *ExecSyncConfig) (*ExecContainerResult, error) {
	ctx, span := c.startSpan(ctx, "ExecSyncContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}
	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.ExecSyncContainer(ctx, func(p proto.Conmon_execSyncContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetId(cfg.ID); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		req.SetTimeoutSec(cfg.Timeout)

		if err := stringSliceToTextList(cfg.Command, req.NewCommand); err != nil {
			return err
		}

		req.SetTerminal(cfg.Terminal)

		if err := stringStringMapToMapEntryList(cfg.EnvVars, req.NewEnvVars); err != nil {
			return err
		}

		c.setCgroupManager(cfg.CgroupManager, req)

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	resp, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	stdout, err := resp.Stdout()
	if err != nil {
		return nil, fmt.Errorf("get stdout: %w", err)
	}

	stderr, err := resp.Stderr()
	if err != nil {
		return nil, fmt.Errorf("get stderr: %w", err)
	}

	execContainerResult := &ExecContainerResult{
		ExitCode: resp.ExitCode(),
		// Copy content of both slices as the `capnp.ReleaseFunc` will
		// zero them when the deferred call to `free()` takes place.
		Stdout:   append(stdout[:0:0], stdout...),
		Stderr:   append(stderr[:0:0], stderr...),
		TimedOut: resp.TimedOut(),
	}

	return execContainerResult, nil
}

func (c *ConmonClient) initLogDrivers(req *proto.Conmon_CreateContainerRequest, logDrivers []ContainerLogDriver) error {
	newLogDrivers, err := req.NewLogDrivers(int32(len(logDrivers)))
	if err != nil {
		return fmt.Errorf("create log drivers: %w", err)
	}

	for i, logDriver := range logDrivers {
		n := newLogDrivers.At(i)
		if logDriver.Type == LogDriverTypeContainerRuntimeInterface {
			n.SetType(proto.Conmon_LogDriver_Type_containerRuntimeInterface)
		}

		if logDriver.Type == LogDriverTypeJSONLogger {
			n.SetType(proto.Conmon_LogDriver_Type_json)
		}

		if logDriver.Type == LogDriverTypeJournald {
			n.SetType(proto.Conmon_LogDriver_Type_journald)
		}

		if err := n.SetPath(logDriver.Path); err != nil {
			return fmt.Errorf("set log driver path: %w", err)
		}

		n.SetMaxSize(logDriver.MaxSize)
	}

	return nil
}

// PID returns the server process ID.
func (c *ConmonClient) PID() uint32 {
	return c.serverPID
}

// Shutdown kill the server via SIGINT. Waits up to 10 seconds for the server
// PID to be removed from the system.
func (c *ConmonClient) Shutdown() error {
	_, span := c.startSpan(context.TODO(), "Shutdown")
	if span != nil {
		defer span.End()
	}

	c.attachReaders.Range(func(_, in any) bool {
		c.closeAttachReader(in)

		return true
	})

	pid := int(c.serverPID)
	if err := syscall.Kill(pid, syscall.SIGINT); err != nil {
		// Process does not exist any more, it might be manually killed.
		if errors.Is(err, syscall.ESRCH) {
			return nil
		}

		return fmt.Errorf("kill server PID: %w", err)
	}

	const (
		waitInterval = 100 * time.Millisecond
		waitCount    = 100
	)

	for range waitCount {
		if err := syscall.Kill(pid, 0); errors.Is(err, syscall.ESRCH) {
			return nil
		}

		time.Sleep(waitInterval)
	}

	return errTimeoutWaitForPid
}

func (c *ConmonClient) pidFile() string {
	return filepath.Join(c.runDir, pidFileName)
}

func (c *ConmonClient) socket() string {
	return filepath.Join(c.runDir, socketName)
}

// ReopenLogContainerConfig is the configuration for calling the
// ReopenLogContainer method.
type ReopenLogContainerConfig struct {
	// ID is the container identifier.
	ID string
}

// ReopenLogContainer can be used to rotate all configured container log
// drivers.
func (c *ConmonClient) ReopenLogContainer(ctx context.Context, cfg *ReopenLogContainerConfig) error {
	ctx, span := c.startSpan(ctx, "ReopenLogContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return fmt.Errorf("create RPC connection: %w", err)
	}

	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.ReopenLogContainer(ctx, func(p proto.Conmon_reopenLogContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetId(cfg.ID); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return fmt.Errorf("create result: %w", err)
	}

	if _, err := result.Response(); err != nil {
		return fmt.Errorf("set response: %w", err)
	}

	return nil
}

type RequestWithMetadata interface {
	NewMetadata(n int32) (proto.Conmon_TextTextMapEntry_List, error)
}

type RequestWithMetadataOld interface {
	RequestWithMetadata
	SetMetadataOld(v []byte) error
}

var (
	_ RequestWithMetadataOld = nil
	// verify that all existing messages are compatible with old conmon-rs servers
	// (new messages don't need to support the old encoding).
	_ = proto.Conmon_VersionRequest{}
	_ = proto.Conmon_CreateContainerRequest{}
	_ = proto.Conmon_ExecSyncContainerRequest{}
	_ = proto.Conmon_AttachRequest{}
	_ = proto.Conmon_ReopenLogRequest{}
	_ = proto.Conmon_SetWindowSizeRequest{}
	_ = proto.Conmon_CreateNamespacesRequest{}
)

// setMetadata sets the tracing metadata properties on the request.
func (c *ConmonClient) setMetadata(ctx context.Context, req RequestWithMetadata) error {
	if !c.tracingEnabled {
		return nil
	}

	span := trace.SpanFromContext(ctx)
	m := make(map[string]string)

	if span.SpanContext().HasSpanID() {
		c.logger.Tracef("Injecting tracing span ID %v", span.SpanContext().SpanID())
		otel.GetTextMapPropagator().Inject(ctx, propagation.MapCarrier(m))
	}

	if err := stringStringMapToMapEntryList(m, req.NewMetadata); err != nil {
		return fmt.Errorf("set metadata2: %w", err)
	}

	// support old conmon-rs servers with json encoded metadata field
	if req, ok := req.(RequestWithMetadataOld); ok {
		metadataBytes, err := json.Marshal(m)
		if err != nil {
			return fmt.Errorf("marshal metadata: %w", err)
		}

		if err := req.SetMetadataOld(metadataBytes); err != nil {
			return fmt.Errorf("set metadata: %w", err)
		}
	}

	return nil
}

// CreateNamespacesConfig is the configuration for calling the
// CreateNamespaces method.
type CreateNamespacesConfig struct {
	// Namespaces are the list of namespaces to unshare.
	Namespaces []Namespace

	// IDMappings are the user and group ID mappings when unsharing the user
	// namespace.
	IDMappings *idtools.IDMappings

	// BasePath is the root path for storing the namespaces.
	// Defaults to "/var/run" if unset.
	BasePath string

	// PodID is the unique identifier of the pod.
	PodID string
}

// CreateNamespacesResponse is the response of the CreateNamespaces method.
type CreateNamespacesResponse struct {
	Namespaces []*NamespacesResponse
}

// NamespacesResponse is the response data for the CreateNamespacesResponse.
type NamespacesResponse struct {
	// Namespace is the type of namespace.
	Type Namespace

	// Path is the base path to the namespaces directory.
	Path string
}

// CreateNamespaces can be used to create a new set of unshared namespaces by
// bind mounting it to the local filesystem.
//
// If a namespace is not selected by the CreateNamespacesConfig, then the
// server will fallback to the host namespace and still create the bind mount
// to it. All namespaces are mounted to /var/run/[ipc,pid,net,user,uts]ns/$POD_ID,
// whereas the POD_ID is being used from the CreateNamespacesConfig as well.
//
// UID and GID mappings are required if unsharing of the user namespace is
// requested.
func (c *ConmonClient) CreateNamespaces(
	ctx context.Context, cfg *CreateNamespacesConfig,
) (*CreateNamespacesResponse, error) {
	ctx, span := c.startSpan(ctx, "CreateNamespaces")
	if span != nil {
		defer span.End()
	}

	// Feature not supported pre v0.5.0
	const minMinor = 5

	minVersion := semver.Version{Minor: minMinor}
	if c.serverVersion.LT(minVersion) {
		return nil, fmt.Errorf("requires at least %v: %w", minVersion, ErrUnsupported)
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}

	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.CreateNamespaces(ctx, func(p proto.Conmon_createNamespaces_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		namespaces, err := req.NewNamespaces(int32(len(cfg.Namespaces)))
		if err != nil {
			return fmt.Errorf("init namespaces: %w", err)
		}

		for i, namespace := range cfg.Namespaces {
			switch namespace {
			case NamespaceIPC:
				namespaces.Set(i, proto.Conmon_Namespace_ipc)

			case NamespaceNet:
				namespaces.Set(i, proto.Conmon_Namespace_net)

			case NamespacePID:
				namespaces.Set(i, proto.Conmon_Namespace_pid)

			case NamespaceUser:
				if cfg.IDMappings == nil ||
					len(cfg.IDMappings.UIDs()) == 0 ||
					len(cfg.IDMappings.GIDs()) == 0 {
					return ErrMissingIDMappings
				}

				namespaces.Set(i, proto.Conmon_Namespace_user)

				if err := stringSliceToTextList(
					mappingsToSlice(cfg.IDMappings.UIDs()),
					req.NewUidMappings,
				); err != nil {
					return fmt.Errorf("convert user ID mappings to text list: %w", err)
				}

				if err := stringSliceToTextList(
					mappingsToSlice(cfg.IDMappings.GIDs()),
					req.NewGidMappings,
				); err != nil {
					return fmt.Errorf("convert group ID mappings to text list: %w", err)
				}

			case NamespaceUTS:
				namespaces.Set(i, proto.Conmon_Namespace_uts)
			}
		}

		if err := req.SetNamespaces(namespaces); err != nil {
			return fmt.Errorf("set namespaces: %w", err)
		}

		if cfg.BasePath == "" {
			cfg.BasePath = "/var/run"
		}

		if err := req.SetBasePath(cfg.BasePath); err != nil {
			return fmt.Errorf("set base path: %w", err)
		}

		if err := req.SetPodId(cfg.PodID); err != nil {
			return fmt.Errorf("set base path: %w", err)
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	response, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	namespaces, err := response.Namespaces()
	if err != nil {
		return nil, fmt.Errorf("set path: %w", err)
	}

	namespacesResponse := []*NamespacesResponse{}

	for i := range namespaces.Len() {
		namespace := namespaces.At(i)

		var typ Namespace

		switch namespace.Type() {
		case proto.Conmon_Namespace_ipc:
			typ = NamespaceIPC

		case proto.Conmon_Namespace_net:
			typ = NamespaceNet

		case proto.Conmon_Namespace_pid:
			typ = NamespacePID

		case proto.Conmon_Namespace_user:
			typ = NamespaceUser

		case proto.Conmon_Namespace_uts:
			typ = NamespaceUTS
		}

		path, err := namespace.Path()
		if err != nil {
			return nil, fmt.Errorf("namespace has no path: %w", err)
		}

		namespacesResponse = append(namespacesResponse,
			&NamespacesResponse{
				Type: typ,
				Path: path,
			},
		)
	}

	return &CreateNamespacesResponse{
		Namespaces: namespacesResponse,
	}, nil
}

func mappingsToSlice(mappings []idtools.IDMap) (res []string) {
	for _, m := range mappings {
		res = append(res, fmt.Sprintf("%d %d %d", m.ContainerID, m.HostID, m.Size))
	}

	return res
}

// ServeExecContainerConfig is the configuration for calling the ServeExecContainer method.
type ServeExecContainerConfig struct {
	// ID is the container identifier.
	ID string

	// Command is the command to be run.
	Command []string

	// Tty indicates if a tty should be used or not.
	Tty bool

	// Stdin indicates if stdin should be available or not.
	Stdin bool

	// Stdout indicates if stdout should be available or not.
	Stdout bool

	// Stderr indicates if stderr should be available or not.
	Stderr bool

	// CgroupManager can be use to select the cgroup manager.
	//
	// To use this option set `ConmonServerConfig.CgroupManager` to
	// `CgroupManagerPerCommand`.
	CgroupManager CgroupManager
}

// ServeExecContainerResult is the result for calling the ServeExecContainer method.
type ServeExecContainerResult struct {
	// URL specifies the returned URL.
	URL string
}

// ServeExecContainer can be used to execute a command within a running container.
func (c *ConmonClient) ServeExecContainer(
	ctx context.Context,
	cfg *ServeExecContainerConfig,
) (*ServeExecContainerResult, error) {
	ctx, span := c.startSpan(ctx, "ServeExecContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}
	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.ServeExecContainer(ctx, func(p proto.Conmon_serveExecContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetId(cfg.ID); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		if err := stringSliceToTextList(cfg.Command, req.NewCommand); err != nil {
			return fmt.Errorf("convert command to text list: %w", err)
		}

		req.SetTty(cfg.Tty)
		req.SetStdin(cfg.Stdin)
		req.SetStdout(cfg.Stdout)
		req.SetStderr(cfg.Stderr)

		c.setCgroupManager(cfg.CgroupManager, req)

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	resp, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	url, err := resp.Url()
	if err != nil {
		return nil, fmt.Errorf("get url: %w", err)
	}

	return &ServeExecContainerResult{URL: url}, nil
}

// ServeAttachContainerConfig is the configuration for calling the ServeAttachContainer method.
type ServeAttachContainerConfig struct {
	// ID is the container identifier.
	ID string

	// Stdin indicates if stdin should be available or not.
	Stdin bool

	// Stdout indicates if stdout should be available or not.
	Stdout bool

	// Stderr indicates if stderr should be available or not.
	Stderr bool

	// CgroupManager can be use to select the cgroup manager.
	//
	// To use this option set `ConmonServerConfig.CgroupManager` to
	// `CgroupManagerPerCommand`.
	CgroupManager CgroupManager
}

// ServeAttachContainerResult is the result for calling the ServeAttachContainer method.
type ServeAttachContainerResult struct {
	// URL specifies the returned URL.
	URL string
}

// ServeAttachContainer can be used to attach to a running container.
func (c *ConmonClient) ServeAttachContainer(
	ctx context.Context,
	cfg *ServeAttachContainerConfig,
) (*ServeAttachContainerResult, error) {
	ctx, span := c.startSpan(ctx, "ServeAttachContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}
	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.ServeAttachContainer(ctx, func(p proto.Conmon_serveAttachContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetId(cfg.ID); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		req.SetStdin(cfg.Stdin)
		req.SetStdout(cfg.Stdout)
		req.SetStderr(cfg.Stderr)

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	resp, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	url, err := resp.Url()
	if err != nil {
		return nil, fmt.Errorf("get url: %w", err)
	}

	return &ServeAttachContainerResult{URL: url}, nil
}

// ServePortForwardContainerConfig is the configuration for calling the ServePortForwardContainer method.
type ServePortForwardContainerConfig struct {
	// NetNsPath is the path to the network namespace of the container.
	NetNsPath string
}

// ServePortForwardContainerResult is the result for calling the ServePortForwardContainer method.
type ServePortForwardContainerResult struct {
	// URL specifies the returned URL.
	URL string
}

// ServePortForwardContainer can be used to forward ports to a running container.
func (c *ConmonClient) ServePortForwardContainer(
	ctx context.Context,
	cfg *ServePortForwardContainerConfig,
) (*ServePortForwardContainerResult, error) {
	ctx, span := c.startSpan(ctx, "ServePortForwardContainer")
	if span != nil {
		defer span.End()
	}

	conn, err := c.newRPCConn()
	if err != nil {
		return nil, fmt.Errorf("create RPC connection: %w", err)
	}
	defer conn.Close()

	client := proto.Conmon(conn.Bootstrap(ctx))

	future, free := client.ServePortForwardContainer(ctx, func(p proto.Conmon_servePortForwardContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return fmt.Errorf("create request: %w", err)
		}

		if err := c.setMetadata(ctx, req); err != nil {
			return err
		}

		if err := req.SetNetNsPath(cfg.NetNsPath); err != nil {
			return fmt.Errorf("set ID: %w", err)
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return nil, fmt.Errorf("create result: %w", err)
	}

	resp, err := result.Response()
	if err != nil {
		return nil, fmt.Errorf("set response: %w", err)
	}

	url, err := resp.Url()
	if err != nil {
		return nil, fmt.Errorf("get url: %w", err)
	}

	return &ServePortForwardContainerResult{URL: url}, nil
}
