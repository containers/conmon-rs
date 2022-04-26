package client

import (
	"context"
	"io"
	"net"

	"github.com/containers/conmon-rs/internal/proto"
	"github.com/containers/podman/v3/libpod/define"
	"github.com/containers/podman/v3/pkg/kubeutils"
	"github.com/containers/podman/v3/utils"
	"github.com/pkg/errors"
	"github.com/sirupsen/logrus"
)

const (
	AttachPipeStdin  = 1
	AttachPipeStdout = 2
	AttachPipeStderr = 3
)

type TerminalSize struct {
	Width, Height uint16
}

type AttachStreams struct {
	Stdin                                   io.Reader
	Stdout, Stderr                          io.WriteCloser
	AttachStdin, AttachStdout, AttachStderr bool
}

type AttachConfig struct {
	ID, SocketPath, ExecSession   string
	Tty, DetachStdin, Passthrough bool
	Resize                        chan define.TerminalSize
	AttachReady                   chan bool
	Streams                       AttachStreams
	StartFunc                     func() error
	DetachKeys                    []byte
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
		// TODO: add exec session
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

	return c.attach(ctx, cfg)
}

func (c *ConmonClient) attach(ctx context.Context, cfg *AttachConfig) error {
	var (
		conn *net.UnixConn
		err  error
	)
	if !cfg.Passthrough {
		logrus.Debugf("Attaching to container %s", cfg.ID)

		kubeutils.HandleResizing(cfg.Resize, func(size define.TerminalSize) {
			logrus.Debugf("Got a resize event: %+v", size)
			if err := c.SetWindowSizeContainer(ctx, &SetWindowSizeContainerConfig{
				ID:     cfg.ID,
				Width:  size.Width,
				Height: size.Height,
			}); err != nil {
				logrus.Debugf("Failed to write to control file to resize terminal: %v", err)
			}
		})

		conn, err = DialLongSocket("unixpacket", cfg.SocketPath)
		if err != nil {
			return errors.Wrapf(err, "failed to connect to container's attach socket: %v", cfg.SocketPath)
		}
		defer func() {
			if err := conn.Close(); err != nil {
				logrus.Errorf("unable to close socket: %q", err)
			}
		}()
	}

	if cfg.StartFunc != nil {
		if err := cfg.StartFunc(); err != nil {
			return err
		}
	}

	if cfg.Passthrough {
		return nil
	}

	receiveStdoutError, stdinDone := setupStdioChannels(cfg, conn)
	if cfg.AttachReady != nil {
		cfg.AttachReady <- true
	}
	return readStdio(cfg, conn, receiveStdoutError, stdinDone)
}
func setupStdioChannels(cfg *AttachConfig, conn *net.UnixConn) (chan error, chan error) {
	receiveStdoutError := make(chan error)
	go func() {
		receiveStdoutError <- redirectResponseToOutputStreams(cfg, conn)
	}()

	stdinDone := make(chan error)
	go func() {
		var err error
		if cfg.Streams.AttachStdin {
			_, err = utils.CopyDetachable(conn, cfg.Streams.Stdin, cfg.DetachKeys)
		}
		stdinDone <- err
	}()

	return receiveStdoutError, stdinDone
}

func redirectResponseToOutputStreams(cfg *AttachConfig, conn io.Reader) error {
	var err error
	buf := make([]byte, 8192+1) /* Sync with conmon STDIO_BUF_SIZE */
	for {
		nr, er := conn.Read(buf)
		if nr > 0 {
			var dst io.Writer
			var doWrite bool
			switch buf[0] {
			case AttachPipeStdout:
				dst = cfg.Streams.Stdout
				doWrite = cfg.Streams.AttachStdout
			case AttachPipeStderr:
				dst = cfg.Streams.Stderr
				doWrite = cfg.Streams.AttachStderr
			default:
				logrus.Infof("Received unexpected attach type %+d", buf[0])
			}
			if dst == nil {
				return errors.New("output destination cannot be nil")
			}

			if doWrite {
				nw, ew := dst.Write(buf[1:nr])
				if ew != nil {
					err = ew
					break
				}
				if nr != nw+1 {
					err = io.ErrShortWrite
					break
				}
			}
		}
		if er == io.EOF {
			break
		}
		if er != nil {
			err = er
			break
		}
	}
	return err
}

func readStdio(cfg *AttachConfig, conn *net.UnixConn, receiveStdoutError, stdinDone chan error) error {
	var err error
	select {
	case err = <-receiveStdoutError:
		conn.CloseWrite()
		return err
	case err = <-stdinDone:
		// This particular case is for when we get a non-tty attach
		// with --leave-stdin-open=true. We want to return as soon
		// as we receive EOF from the client. However, we should do
		// this only when stdin is enabled. If there is no stdin
		// enabled then we wait for output as usual.
		if cfg.DetachStdin {
			return nil
		}
		if err == define.ErrDetach {
			conn.CloseWrite()
			return err
		}
		if err == nil {
			// copy stdin is done, close it
			if connErr := conn.CloseWrite(); connErr != nil {
				logrus.Errorf("Unable to close conn: %v", connErr)
			}
		}
		if cfg.Streams.AttachStdout || cfg.Streams.AttachStderr {
			return <-receiveStdoutError
		}
	}
	return nil
}

type SetWindowSizeContainerConfig struct {
	ID     string
	Width  uint16
	Height uint16
}

func (c *ConmonClient) SetWindowSizeContainer(ctx context.Context, cfg *SetWindowSizeContainerConfig) error {
	conn, err := c.newRPCConn()
	if err != nil {
		return err
	}
	defer conn.Close()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.SetWindowSizeContainer(ctx, func(p proto.Conmon_setWindowSizeContainer_Params) error {
		req, err := p.NewRequest()
		if err != nil {
			return err
		}
		if err := req.SetId(cfg.ID); err != nil {
			return err
		}
		req.SetWidth(cfg.Width)
		req.SetHeight(cfg.Height)
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
