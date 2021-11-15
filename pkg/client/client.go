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

	"capnproto.org/go/capnp/v3/rpc"
	"github.com/containers/conmon-rs/internal/proto"
)

const (
	binaryName = "conmon-server"
)

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
	client := proto.Conmon{Client: conn.Bootstrap(context.Background())}

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
		return "", args, errors.New("Runtime must be specified")
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
	return entrypoint, args, nil
}
