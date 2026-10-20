package client

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"syscall"

	"github.com/containers/conmon-rs/internal/proto"
	"github.com/google/uuid"
	"go.podman.io/common/pkg/detach"
	"go.podman.io/common/pkg/resize"
)

const (
	attachPacketBufSize = 8192
	attachPipeDone      = 0
	attachPipeStdin     = 1 //nolint:deadcode,varcheck // Not used right now
	attachPipeStdout    = 2
	attachPipeStderr    = 3
)

var errTerminalSizeNil = errors.New("terminal size cannot be nil")

// AttachStreams are the stdio streams for the AttachConfig.
type AttachStreams struct {
	// Standard input stream, can be nil.
	Stdin *In

	// Standard output stream, can be nil.
	Stdout *Out

	// Standard error stream, can be nil.
	Stderr *Out
}

// In defines an input stream.
type In struct {
	// Wraps an io.ReadCloser
	io.ReadCloser
}

// Out defines an output stream.
type Out struct {
	// Wraps an io.WriteCloser
	io.WriteCloser
}

// AttachConfig is the configuration for running the Attach method.
type AttachConfig struct {
	// ID of the container.
	ID string

	// Path of the attach socket.
	SocketPath string

	// ExecSession ID, if this is an attach for an Exec.
	ExecSession string

	// Whether a terminal was setup for the command this is attaching to.
	Tty bool

	// Whether the container stdin should be closed once the attach stdin
	// reaches EOF (stdin once). Without a terminal, the output is still
	// forwarded until the container closes it. A terminal is never closed,
	// so the attach returns on stdin EOF.
	//
	// The server applies the value of the latest attach call to all
	// connections it accepts afterwards on the same socket, so all attaches
	// of a container should use the same value, for example derived from
	// its config.
	StopAfterStdinEOF bool

	// Whether the container supports stdin or not.
	ContainerStdin bool

	// Whether the output is passed through the caller's std streams, rather than
	// ones created for the attach session.
	Passthrough bool

	// Channel of resize events.
	Resize <-chan resize.TerminalSize

	// The standard streams for this attach session.
	Streams AttachStreams

	// A closure to be run before the streams are attached.
	// This could be used to start a container.
	PreAttachFunc func() error

	// A closure to be run after the streams are attached.
	// This could be used to notify callers the streams have been attached.
	PostAttachFunc func() error

	// The keys that indicate the attach session should be detached.
	DetachKeys []byte
}

// attachReaderValue is the value of the attachReaders map.
type attachReaderValue struct {
	stdin      *In
	socketPath string
}

// AttachContainer can be used to attach to a running container.
func (c *ConmonClient) AttachContainer(ctx context.Context, cfg *AttachConfig) error {
	ctx, span := c.startSpan(ctx, "AttachContainer")
	if span != nil {
		defer span.End()
	}

	rpcClient, err := c.client(ctx)
	if err != nil {
		return fmt.Errorf("create RPC client: %w", err)
	}

	future, free := rpcClient.AttachContainer(ctx, func(p proto.Conmon_attachContainer_Params) error {
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

		if err := req.SetSocketPath(cfg.SocketPath); err != nil {
			return fmt.Errorf("set socket path: %w", err)
		}

		req.SetStopAfterStdinEof(cfg.StopAfterStdinEOF)

		if cfg.ExecSession != "" {
			if err := req.SetExecSessionId(cfg.ExecSession); err != nil {
				return fmt.Errorf("set exec session ID: %w", err)
			}
		}

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		c.resetConn()

		return fmt.Errorf("create result: %w", err)
	}

	if _, err := result.Response(); err != nil {
		return fmt.Errorf("set response: %w", err)
	}

	if err := c.attach(ctx, cfg); err != nil {
		return fmt.Errorf("run attach: %w", err)
	}

	return nil
}

func (c *ConmonClient) attach(ctx context.Context, cfg *AttachConfig) (err error) {
	var conn *net.UnixConn

	if !cfg.Passthrough {
		c.logger.Debugf("Attaching to container %s", cfg.ID)

		resize.HandleResizing(cfg.Resize, func(size resize.TerminalSize) {
			c.logger.Debugf("Got a resize event: %+v", size)

			if err := c.SetWindowSizeContainer(ctx, &SetWindowSizeContainerConfig{
				ID:   cfg.ID,
				Size: &size,
			}); err != nil {
				c.logger.Debugf("Failed to write to control file to resize terminal: %v", err)
			}
		})

		conn, err = DialLongSocket("unixpacket", cfg.SocketPath)
		if err != nil {
			return fmt.Errorf("failed to connect to container's attach socket: %v: %w", cfg.SocketPath, err)
		}

		defer func() {
			if err := conn.Close(); err != nil {
				c.logger.Errorf("unable to close socket: %q", err)
			}
		}()
	}

	if cfg.PreAttachFunc != nil {
		if err := cfg.PreAttachFunc(); err != nil {
			return fmt.Errorf("run pre attach func: %w", err)
		}
	}

	if cfg.Passthrough {
		return nil
	}

	id := uuid.NewString()

	receiveStdoutError, stdinDone := c.setupStdioChannels(ctx, cfg, conn, id)

	if cfg.PostAttachFunc != nil {
		if err := cfg.PostAttachFunc(); err != nil {
			return fmt.Errorf("run post attach func: %w", err)
		}
	}

	if err := c.readStdio(ctx, cfg, conn, id, receiveStdoutError, stdinDone); err != nil {
		return fmt.Errorf("read stdio: %w", err)
	}

	return nil
}

func (c *ConmonClient) setupStdioChannels(
	ctx context.Context, cfg *AttachConfig, conn *net.UnixConn, id string,
) (receiveStdoutError, stdinDone chan error) {
	ctx, span := c.startSpan(ctx, "setupStdioChannels")
	if span != nil {
		defer span.End()
	}

	// Buffered, so that the goroutines can always finish, even if nobody
	// receives their result anymore.
	receiveStdoutError = make(chan error, 1)

	go func() {
		receiveStdoutError <- c.redirectResponseToOutputStreams(ctx, cfg, conn)
	}()

	stdinDone = make(chan error, 1)
	stdin := cfg.Streams.Stdin

	if stdin != nil {
		value := &attachReaderValue{stdin: stdin, socketPath: cfg.SocketPath}
		c.attachReaders.Store(id, value)

		go func() {
			_, err := detach.Copy(conn, stdin, cfg.DetachKeys)
			if !errors.Is(err, io.ErrClosedPipe) {
				stdinDone <- err
			}
		}()
	}

	return receiveStdoutError, stdinDone
}

func (c *ConmonClient) redirectResponseToOutputStreams(
	ctx context.Context, cfg *AttachConfig, conn io.Reader,
) (err error) {
	ctx, span := c.startSpan(ctx, "redirectResponseToOutputStreams")
	if span != nil {
		defer span.End()
	}

	buf := make([]byte, attachPacketBufSize+1) /* Sync with conmonrs ATTACH_PACKET_BUF_SIZE */

	defer func() {
		if cfg.Streams.Stdout != nil {
			cfg.Streams.Stdout.Close()
		}

		if cfg.Streams.Stderr != nil {
			cfg.Streams.Stderr.Close()
		}
	}()

	for {
		c.logger.Trace("Waiting to read from attach connection")

		nr, er := conn.Read(buf)
		c.logger.WithError(er).Tracef("Got %d bytes from attach connection", nr)

		if nr > 0 {
			cont, er := c.handlePacket(ctx, cfg, buf, nr)
			if er != nil {
				return er
			}

			if !cont {
				return nil
			}
		}

		if er == io.EOF {
			return nil
		}

		if errors.Is(er, syscall.ECONNRESET) {
			c.logger.WithError(er).Trace("Connection reset, retrying to read")

			continue
		}

		if er != nil {
			err = er

			break
		}
	}

	if err != nil {
		return fmt.Errorf("redirect response to output streams: %w", err)
	}

	return nil
}

func (c *ConmonClient) handlePacket(
	ctx context.Context, cfg *AttachConfig, buf []byte, nr int,
) (cont bool, err error) {
	_, span := c.startSpan(ctx, "handlePacket")
	if span != nil {
		defer span.End()
	}

	var dst io.Writer

	switch buf[0] {
	case attachPipeDone:
		c.logger.Trace("Received done packet")

		return false, nil

	case attachPipeStdout:
		if cfg.Streams.Stdout == nil {
			c.logger.Debug("stdout for packet is nil")

			return true, nil
		}

		dst = cfg.Streams.Stdout

		c.logger.Trace("Received stdout packet")

	case attachPipeStderr:
		if cfg.Streams.Stderr == nil {
			c.logger.Debug("stderr for packet is nil")

			return true, nil
		}

		dst = cfg.Streams.Stderr

		c.logger.Trace("Received stderr packet")

	default:
		c.logger.Infof("Received unexpected attach type %+d", buf[0])

		return true, nil
	}

	nw, ew := dst.Write(buf[1:nr])
	c.logger.WithError(ew).Tracef("Wrote %d bytes to destination", nw)

	if ew != nil {
		return false, fmt.Errorf("failed to write packet %w", ew)
	}

	if nr != nw+1 {
		return false, io.ErrShortWrite
	}

	return true, nil
}

func (c *ConmonClient) tryCloseAttachReaderForID(id string) {
	c.logger.Tracef("Closing attach reader for ID: %s", id)

	if val, ok := c.attachReaders.LoadAndDelete(id); ok {
		c.closeAttachReader(val)
	}
}

func (c *ConmonClient) closeAttachReader(val any) {
	value, ok := val.(*attachReaderValue)
	if !ok {
		c.logger.Error("Ignoring input value of wrong type")

		return
	}

	if err := value.stdin.Close(); err != nil {
		c.logger.WithError(err).Warn("Unable to close attach reader")
	}

	// Check if the attach socket is still in use by other connections
	socketPathInUse := false

	c.attachReaders.Range(func(k, v any) bool {
		existing, ok := v.(*attachReaderValue)
		if !ok {
			return true
		}

		if existing.socketPath == value.socketPath {
			socketPathInUse = true

			return false
		}

		return true
	})

	if !socketPathInUse {
		c.logger.Infof("Attach socket path no longer in use, removing it: %s", value.socketPath)

		if err := os.RemoveAll(value.socketPath); err != nil {
			c.logger.WithError(err).Warn("Unable to remove attach socket path")
		}
	}
}

func (c *ConmonClient) readStdio(
	ctx context.Context, cfg *AttachConfig, conn *net.UnixConn, id string, receiveStdoutError, stdinDone chan error,
) (err error) {
	_, span := c.startSpan(ctx, "readStdio")
	if span != nil {
		defer span.End()
	}

	c.logger.Trace("Read stdio on attach")

	select {
	case err = <-receiveStdoutError:
		c.logger.WithError(err).Trace("Received message on output channel")
		c.tryCloseAttachReaderForID(id)

		if closeErr := conn.CloseWrite(); closeErr != nil {
			return errors.Join(closeErr, err)
		}

		if err != nil {
			return fmt.Errorf("got stdout error: %w", err)
		}

		return nil

	case err = <-stdinDone:
		c.logger.WithError(err).Trace("Received message on input channel")

		c.tryCloseAttachReaderForID(id)

		// Copying stdin is done, close it. With StopAfterStdinEOF the server
		// closes the container stdin in turn, so the output written
		// afterwards still has to be received below.
		closeErr := conn.CloseWrite()

		// A detach or a failed stdin copy ends the attach right away.
		if err != nil {
			if !errors.Is(err, detach.ErrDetach) {
				err = fmt.Errorf("copy stdin: %w", err)
			}

			return errors.Join(closeErr, err)
		}

		if closeErr != nil {
			c.logger.Errorf("Unable to close conn: %v", closeErr)
		}

		// The server never closes a terminal, and closes the container stdin
		// without one only with StopAfterStdinEOF. The EOF of the client then
		// only ends this attach session, so stop receiving the output as
		// well, so that nothing gets written to the output streams after the
		// attach returned.
		if (cfg.Tty && cfg.StopAfterStdinEOF) || (!cfg.Tty && cfg.ContainerStdin && !cfg.StopAfterStdinEOF) {
			if closeErr := conn.CloseRead(); closeErr != nil {
				c.logger.Errorf("Unable to close conn: %v", closeErr)
			}

			if err := waitForOutput(ctx, receiveStdoutError); err != nil {
				c.logger.WithError(err).Debug("Output failed after stdin EOF")
			}

			return nil
		}

		if cfg.Streams.Stdout != nil || cfg.Streams.Stderr != nil {
			return waitForOutput(ctx, receiveStdoutError)
		}
	}

	return nil
}

// waitForOutput waits until the output got received or the context is done.
func waitForOutput(ctx context.Context, receiveStdoutError <-chan error) error {
	select {
	case err := <-receiveStdoutError:
		return err
	case <-ctx.Done():
		return fmt.Errorf("wait for output: %w", ctx.Err())
	}
}

// SetWindowSizeContainerConfig is the configuration for calling the SetWindowSizeContainer method.
type SetWindowSizeContainerConfig struct {
	// ID specifies the container ID.
	ID string

	// Size is the new terminal size.
	Size *resize.TerminalSize
}

// SetWindowSizeContainer can be used to change the window size of a running container.
func (c *ConmonClient) SetWindowSizeContainer(ctx context.Context, cfg *SetWindowSizeContainerConfig) error {
	ctx, span := c.startSpan(ctx, "SetWindowSizeContainer")
	if span != nil {
		defer span.End()
	}

	if cfg.Size == nil {
		return errTerminalSizeNil
	}

	rpcClient, err := c.client(ctx)
	if err != nil {
		return fmt.Errorf("create RPC client: %w", err)
	}

	future, free := rpcClient.SetWindowSizeContainer(ctx, func(p proto.Conmon_setWindowSizeContainer_Params) error {
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

		req.SetWidth(cfg.Size.Width)
		req.SetHeight(cfg.Size.Height)

		return nil
	})
	defer free()

	result, err := future.Struct()
	if err != nil {
		c.resetConn()

		return fmt.Errorf("create result: %w", err)
	}

	if _, err := result.Response(); err != nil {
		return fmt.Errorf("set response: %w", err)
	}

	return nil
}
