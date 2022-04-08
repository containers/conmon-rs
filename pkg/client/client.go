package client

import (
	"context"
	"fmt"
	"io"
	"io/ioutil"
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
)

const (
	binaryName  = "conmonrs"
	socketName  = "conmon.sock"
	pidFileName = "pidfile"
)

type ConmonClient struct {
	serverPID uint32
	runDir    string
}

type ConmonServerConfig struct {
	ConmonServerPath string
	LogLevel         string
	Runtime          string
	RuntimeRoot      string
	ServerRunDir     string
	LogDriver        string
	Stdout           io.WriteCloser
	Stderr           io.WriteCloser
}

func New(config *ConmonServerConfig) (_ *ConmonClient, retErr error) {
	cl, err := config.ToClient()
	if err != nil {
		return nil, err
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
			cl.Shutdown()
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

	return &ConmonClient{
		runDir: c.ServerRunDir,
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

func (c *ConmonClient) toArgs(config *ConmonServerConfig) (string, []string, error) {
	const maxUnixSocketPathSize = len(syscall.RawSockaddrUnix{}.Path)
	args := make([]string, 0)
	if c == nil {
		return "", args, nil
	}
	entrypoint := config.ConmonServerPath
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

	// TODO FIXME do validation?
	if config.LogLevel != "" {
		args = append(args, "--log-level", config.LogLevel)
	}
	if config.LogDriver != "" {
		args = append(args, "--log-driver", config.LogDriver)
	}

	return entrypoint, args, nil
}

func pidGivenFile(file string) (uint32, error) {
	pidBytes, err := ioutil.ReadFile(file)
	if err != nil {
		return 0, fmt.Errorf("reading pid bytes: %w", err)
	}
	pidU64, err := strconv.ParseUint(string(pidBytes), 10, 32)
	if err != nil {
		return 0, fmt.Errorf("parsing pid: %w", err)
	}
	return uint32(pidU64), nil
}

func (c *ConmonClient) waitUntilServerUp() error {
	var err error
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
func DialLongSocket(network, path string) (net.Conn, error) {
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

func (c *ConmonClient) Version(ctx context.Context) (string, error) {
	conn, err := c.newRPCConn()
	if err != nil {
		return "", err
	}
	defer conn.Close()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.Version(ctx, nil)
	defer free()

	result, err := future.Struct()
	if err != nil {
		return "", err
	}

	response, err := result.Response()
	if err != nil {
		return "", err
	}
	return response.Version()
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

func (c *ConmonClient) CreateContainer(ctx context.Context, cfg *CreateContainerConfig) (*CreateContainerResponse, error) {
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
		switch logDriver.Type {
		case LogDriverTypeContainerRuntimeInterface:
			n.SetType(proto.Conmon_LogDriver_Type_containerRuntimeInterface)
		}
		n.SetPath(logDriver.Path)

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

type AttachConfig struct {
	ID         string
	SocketPath string
}

func (c *ConmonClient) AttachContainer(ctx context.Context, cfg *AttachConfig) error {
	conn, err := c.newRPCConn()
	if err != nil {
		return err
	}
	defer conn.Close()

	client := proto.Conmon{Client: conn.Bootstrap(ctx)}
	future, free := client.AttachContainer(ctx, func(p proto.Conmon_attachContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return err
		}
		if err := req.SetId(cfg.ID); err != nil {
			return err
		}
		if err := req.SetSocketPath(cfg.SocketPath); err != nil {
			return err
		}
		return nil
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
