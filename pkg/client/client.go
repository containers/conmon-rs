package client

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/ioutil"
	"net"
	"os"
	"os/exec"
	"strconv"
	"syscall"

	"capnproto.org/go/capnp/v3"
	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
)

const binaryName = "conmon"

type ConmonClient struct {
	conmonPID uint32
	socket    string
}

type ConmonServerConfig struct {
	ConmonServerPath string
	LogLevel         string
	ConmonPIDFile    string
	Runtime          string
	Socket           string
	RuntimeRoot      string
	Stdin            io.Reader
	Stdout           io.WriteCloser
	Stderr           io.WriteCloser
}

func New(config *ConmonServerConfig) (_ *ConmonClient, retErr error) {
	entrypoint, args, err := config.ToArgs()
	if err != nil {
		return nil, err
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
	if err := cmd.Run(); err != nil {
		return nil, err
	}
	pid, err := pidGivenFile(config.ConmonPIDFile)
	if err != nil {
		return nil, err
	}
	cl := &ConmonClient{
		conmonPID: pid,
		socket:    config.Socket,
	}

	// Cleanup the background server process
	// if we fail any of the next steps
	defer func() {
		if retErr != nil {
			cl.Shutdown()
		}
	}()
	if err := os.Remove(config.ConmonPIDFile); err != nil {
		return nil, err
	}
	return cl, nil
}

func pidGivenFile(file string) (uint32, error) {
	pidBytes, err := ioutil.ReadFile(file)
	if err != nil {
		return 0, err
	}
	pidU64, err := strconv.ParseUint(string(pidBytes), 10, 32)
	if err != nil {
		return 0, err
	}
	return uint32(pidU64), nil
}

func (c *ConmonClient) newRPCConn() (*rpc.Conn, error) {
	socketConn, err := net.Dial("unix", c.socket)
	if err != nil {
		return nil, err
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
	Stdout   string
	Stderr   string
}

func (c *ConmonClient) ExecSyncContainer(ctx context.Context, id string, command []string, timeout int32) (*ExecContainerResult, error) {
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
		req.SetTimeout(timeout)
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
	return c.conmonPID
}

func (c *ConmonClient) Shutdown() error {
	return syscall.Kill(int(c.conmonPID), syscall.SIGINT)
}

func (c *ConmonServerConfig) ToArgs() (string, []string, error) {
	const maxUnixSocketPathSize = len(syscall.RawSockaddrUnix{}.Path)
	args := make([]string, 0)
	if c == nil {
		return "", args, nil
	}
	entrypoint := c.ConmonServerPath
	if entrypoint == "" {
		path, err := exec.LookPath(binaryName)
		if err != nil {
			return "", args, err
		}
		entrypoint = path
	}
	if c.Runtime == "" {
		return "", args, errors.New("runtime must be specified")
	}
	args = append(args, "--runtime", c.Runtime)

	// TODO FIXME do validation?
	if c.LogLevel != "" {
		args = append(args, "--log-level", c.LogLevel)
	}
	// TODO FIXME probably fail otherwise we leak processes
	if c.ConmonPIDFile != "" {
		args = append(args, "--conmon-pidfile", c.ConmonPIDFile)
	}
	if c.Socket != "" {
		if len(c.Socket) > maxUnixSocketPathSize {
			return "", args, fmt.Errorf("unix socket path %q is too long", c.Socket)
		}
		args = append(args, "--socket", c.Socket)
	}
	if c.RuntimeRoot != "" {
		args = append(args, "--runtime-root", c.RuntimeRoot)
	}
	return entrypoint, args, nil
}
