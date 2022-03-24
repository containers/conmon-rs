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

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
)

const binaryName = "conmon"

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
	Stdin            io.Reader
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
	if config.Stdin != nil {
		cmd.Stdin = os.Stdin
	}
	if config.Stdout != nil {
		cmd.Stdout = os.Stdout
	}
	if config.Stderr != nil {
		cmd.Stderr = os.Stderr
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

	if config.RuntimeRoot != "" {
		args = append(args, "--runtime-root", config.RuntimeRoot)
	}

	// TODO FIXME do validation?
	if config.LogLevel != "" {
		args = append(args, "--log-level", config.LogLevel)
	}
	args = append(args, "--conmon-pidfile", c.pidFile())

	if len(c.socket()) > maxUnixSocketPathSize {
		return "", args, fmt.Errorf("unix socket path %q is too long", c.socket())
	}
	args = append(args, "--socket", c.socket())
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

func (c *ConmonClient) newRPCConn() (*rpc.Conn, error) {
	socketConn, err := net.Dial("unix", c.socket())
	if err != nil {
		return nil, fmt.Errorf("new RPC conn: %w", err)
	}

	return rpc.NewConn(rpc.NewStreamTransport(socketConn), nil), nil
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
}

func (c *ConmonClient) CreateContainer(ctx context.Context, cfg *CreateContainerConfig) (uint32, error) {
	conn, err := c.newRPCConn()
	if err != nil {
		return 0, err
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
		return p.SetRequest(req)
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		return 0, err
	}

	response, err := result.Response()
	if err != nil {
		return 0, err
	}
	return response.ContainerPid(), nil
}

type ExecContainerResult struct {
	ExitCode int32
	Stdout   []byte
	Stderr   []byte
}

func (c *ConmonClient) ExecSyncContainer(ctx context.Context, id string, command []string, timeoutSec uint64) (*ExecContainerResult, error) {
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
		if err := req.SetId(id); err != nil {
			return err
		}
		req.SetTimeoutSec(timeoutSec)
		if err := stringSliceToTextList(command, req.NewCommand); err != nil {
			return err
		}
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

// TODO FIXME test only?
func (c *ConmonClient) PID() uint32 {
	return c.serverPID
}

func (c *ConmonClient) Shutdown() error {
	return syscall.Kill(int(c.serverPID), syscall.SIGINT)
}

func (c *ConmonClient) socket() string {
	return filepath.Join(c.runDir, "conmon.sock")
}

func (c *ConmonClient) pidFile() string {
	return filepath.Join(c.runDir, "pidfile")
}
