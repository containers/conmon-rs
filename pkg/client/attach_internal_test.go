package client

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/sirupsen/logrus"
	"go.podman.io/common/pkg/detach"
	"go.podman.io/common/pkg/resize"
)

// testOut is an output stream which records what got written and whether
// it got closed.
type testOut struct {
	mu     sync.Mutex
	buf    bytes.Buffer
	closed bool
	err    error

	// delay slows down every write, if set.
	delay time.Duration
}

func (o *testOut) Write(b []byte) (int, error) {
	time.Sleep(o.delay)

	o.mu.Lock()
	defer o.mu.Unlock()

	if o.err != nil {
		return 0, o.err
	}

	o.buf.Write(b)

	return len(b), nil
}

func (o *testOut) Close() error {
	o.mu.Lock()
	defer o.mu.Unlock()

	o.closed = true

	return nil
}

func (o *testOut) state() (string, bool) {
	o.mu.Lock()
	defer o.mu.Unlock()

	return o.buf.String(), o.closed
}

// shortOut writes one byte less than asked for.
type shortOut struct{}

func (shortOut) Write(b []byte) (int, error) { return len(b) - 1, nil }
func (shortOut) Close() error                { return nil }

func TestHandlePacket(t *testing.T) {
	t.Parallel()

	c := &ConmonClient{logger: logrus.New()}
	stdout, stderr := &testOut{}, &testOut{}
	cfg := &AttachConfig{Streams: AttachStreams{Stdout: &Out{stdout}, Stderr: &Out{stderr}}}

	for _, tc := range []struct {
		packet []byte
		cont   bool
	}{
		{packet: []byte{attachPipeStdout, 'o', 'u', 't'}, cont: true},
		{packet: []byte{attachPipeStderr, 'e', 'r', 'r'}, cont: true},
		{packet: []byte{42, 'x'}, cont: true},
		{packet: []byte{attachPipeDone}, cont: false},
	} {
		cont, err := c.handlePacket(cfg, tc.packet, len(tc.packet), true)
		if err != nil || cont != tc.cont {
			t.Fatalf("packet %v: got %v, %v, want %v", tc.packet, cont, err, tc.cont)
		}
	}

	if out, _ := stdout.state(); out != "out" {
		t.Errorf("got stdout %q, want %q", out, "out")
	}

	if out, _ := stderr.state(); out != "err" {
		t.Errorf("got stderr %q, want %q", out, "err")
	}

	// Output for a stream which is not attached gets dropped.
	packet := []byte{attachPipeStdout, 'x'}

	cont, err := c.handlePacket(&AttachConfig{}, packet, len(packet), true)
	if err != nil || !cont {
		t.Fatalf("got %v, %v for a missing stream, want to continue", cont, err)
	}

	cfg = &AttachConfig{Streams: AttachStreams{Stdout: &Out{&testOut{err: io.ErrClosedPipe}}}}

	if _, err := c.handlePacket(cfg, packet, len(packet), true); !errors.Is(err, io.ErrClosedPipe) {
		t.Fatalf("got error %v, want %v", err, io.ErrClosedPipe)
	}

	cfg = &AttachConfig{Streams: AttachStreams{Stdout: &Out{shortOut{}}}}

	if _, err := c.handlePacket(cfg, packet, len(packet), true); !errors.Is(err, io.ErrShortWrite) {
		t.Fatalf("got error %v, want %v", err, io.ErrShortWrite)
	}
}

func TestRedirectResponseToOutputStreams(t *testing.T) {
	t.Parallel()

	server, client := packetPair(t)

	for _, packet := range [][]byte{
		{attachPipeStdout, 'a'},
		{attachPipeStderr, 'b'},
		{attachPipeStdout, 'c'},
		{attachPipeDone},
		// Not read anymore after the done packet.
		{attachPipeStdout, 'd'},
	} {
		if _, err := server.Write(packet); err != nil {
			t.Fatalf("write packet: %v", err)
		}
	}

	c := &ConmonClient{logger: logrus.New()}
	stdout, stderr := &testOut{}, &testOut{}
	cfg := &AttachConfig{Streams: AttachStreams{Stdout: &Out{stdout}, Stderr: &Out{stderr}}}

	if err := c.redirectResponseToOutputStreams(t.Context(), cfg, client); err != nil {
		t.Fatalf("redirect: %v", err)
	}

	for name, tc := range map[string]struct {
		out  *testOut
		want string
	}{
		"stdout": {stdout, "ac"},
		"stderr": {stderr, "b"},
	} {
		out, closed := tc.out.state()
		if out != tc.want || !closed {
			t.Errorf("%s: got %q, closed %v, want %q, closed", name, out, closed, tc.want)
		}
	}

	// The end of the connection ends the output as well.
	server.Close()

	stdout = &testOut{}
	cfg = &AttachConfig{Streams: AttachStreams{Stdout: &Out{stdout}}}

	if err := c.redirectResponseToOutputStreams(t.Context(), cfg, client); err != nil {
		t.Fatalf("redirect after close: %v", err)
	}

	if _, closed := stdout.state(); !closed {
		t.Error("stdout not closed at the end of the connection")
	}
}

func TestStdinEOFEndsAttachOnlyWhenOutputCannotEnd(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		tty, stopAfterStdinEOF, containerStdin bool
		// Whether the attach returns on the EOF of its stdin, otherwise it
		// keeps receiving the output until it ends.
		returns bool
	}{
		// The container stdin stays open, its output does not end with
		// the stdin of the client.
		{containerStdin: true, returns: true},
		// The server closes the container stdin, which ends the output of
		// a process which reads it.
		{stopAfterStdinEOF: true, containerStdin: true},
		{stopAfterStdinEOF: true},
		{},
		// A terminal never gets closed.
		{tty: true, stopAfterStdinEOF: true, returns: true},
		{tty: true, stopAfterStdinEOF: true, containerStdin: true, returns: true},
		{tty: true},
		{tty: true, containerStdin: true},
	} {
		name := fmt.Sprintf(
			"tty %v stop after stdin EOF %v container stdin %v",
			tc.tty, tc.stopAfterStdinEOF, tc.containerStdin,
		)
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			c := newTestClient(t, t.TempDir(), logrus.New())
			t.Cleanup(func() { c.Close() })

			stdin, stdinWriter := io.Pipe()
			stdout := &testOut{}
			cfg := &AttachConfig{
				ID:                "id",
				SocketPath:        serveAttachSocket(t),
				Tty:               tc.tty,
				StopAfterStdinEOF: tc.stopAfterStdinEOF,
				ContainerStdin:    tc.containerStdin,
				Streams:           AttachStreams{Stdin: &In{stdin}, Stdout: &Out{stdout}},
			}

			ctx, cancel := context.WithCancel(t.Context())
			defer cancel()

			attachErr := make(chan error, 1)

			go func() { attachErr <- c.attach(ctx, cfg) }()

			waitFor(t, "no output", func() bool {
				out, _ := stdout.state()

				return out != ""
			})

			stdinWriter.Close()

			if tc.returns {
				select {
				case err := <-attachErr:
					if err != nil {
						t.Fatalf("attach failed: %v", err)
					}
				case <-time.After(10 * time.Second):
					t.Fatal("attach still running after the EOF of its stdin")
				}

				return
			}

			// The server keeps sending output.
			select {
			case err := <-attachErr:
				t.Fatalf("attach ended with %v on the EOF of its stdin", err)
			case <-time.After(200 * time.Millisecond):
			}

			cancel()

			if err := <-attachErr; !errors.Is(err, context.Canceled) {
				t.Fatalf("got error %v, want %v", err, context.Canceled)
			}
		})
	}
}

// serveAttachSocket listens like the attach socket of a container and sends
// output on every connection until it gets closed.
func serveAttachSocket(t *testing.T) string {
	t.Helper()

	path := filepath.Join(t.TempDir(), "attach")

	listener, err := net.ListenUnix("unixpacket", &net.UnixAddr{Name: path, Net: "unixpacket"})
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	t.Cleanup(func() { listener.Close() })

	go func() {
		for {
			conn, err := listener.AcceptUnix()
			if err != nil {
				return
			}

			go func() {
				defer conn.Close()

				for {
					if _, err := conn.Write([]byte{attachPipeStdout, 'x'}); err != nil {
						return
					}

					time.Sleep(10 * time.Millisecond)
				}
			}()
		}
	}()

	return path
}

func TestCloseEndsAttach(t *testing.T) {
	t.Parallel()

	for _, withStdin := range []bool{false, true} {
		t.Run("stdin "+strconv.FormatBool(withStdin), func(t *testing.T) {
			t.Parallel()

			c := newTestClient(t, t.TempDir(), logrus.New())

			stdout, stdoutWriter := io.Pipe()
			cfg := &AttachConfig{
				ID:         "id",
				SocketPath: serveAttachSocket(t),
				Streams:    AttachStreams{Stdout: &Out{stdoutWriter}},
			}

			if withStdin {
				stdin, _ := io.Pipe()
				cfg.Streams.Stdin = &In{stdin}
			}

			attachErr := make(chan error, 1)

			go func() { attachErr <- c.attach(t.Context(), cfg) }()

			// The attach is running once the output arrives.
			if _, err := io.ReadFull(stdout, make([]byte, 1)); err != nil {
				t.Fatalf("read output: %v", err)
			}

			// The output stream gets closed.
			drained := make(chan error, 1)

			go func() {
				_, err := io.Copy(io.Discard, stdout)
				drained <- err
			}()

			if err := c.Close(); err != nil {
				t.Fatalf("close: %v", err)
			}

			select {
			case err := <-attachErr:
				if !errors.Is(err, ErrClientClosed) {
					t.Fatalf("got error %v, want %v", err, ErrClientClosed)
				}
			case <-time.After(10 * time.Second):
				t.Fatal("attach still running after Close")
			}

			if err := <-drained; err != nil {
				t.Fatalf("drain output: %v", err)
			}
		})
	}
}

func TestClosedFileStdinKeepsAttach(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	stdin, stdinWriter, err := os.Pipe()
	if err != nil {
		t.Fatalf("pipe: %v", err)
	}

	t.Cleanup(func() { stdinWriter.Close() })

	stdout, stdoutWriter := io.Pipe()
	cfg := &AttachConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: &In{stdin}, Stdout: &Out{stdoutWriter}},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	// The attach is running once the output arrives.
	if _, err := io.ReadFull(stdout, make([]byte, 1)); err != nil {
		t.Fatalf("read output: %v", err)
	}

	go func() { _, _ = io.Copy(io.Discard, stdout) }() //nolint:errcheck // drained only

	// Like on Shutdown, a read of the closed file fails with os.ErrClosed,
	// which does not end the attach.
	c.closeAttachStdins()

	select {
	case err := <-attachErr:
		t.Fatalf("attach ended with %v after its stdin got closed", err)
	case <-time.After(200 * time.Millisecond):
	}

	if err := c.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if err := <-attachErr; !errors.Is(err, ErrClientClosed) {
		t.Fatalf("got error %v, want %v", err, ErrClientClosed)
	}
}

func TestDetachStopsOutput(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	stdin, stdinWriter := io.Pipe()
	// A slow stream, which is still being written to on the detach.
	stdout := &testOut{delay: 20 * time.Millisecond}
	cfg := &AttachConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: &In{stdin}, Stdout: &Out{stdout}},
		DetachKeys: []byte{16, 17},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	waitFor(t, "no output", func() bool {
		out, _ := stdout.state()

		return out != ""
	})

	// The keys get read one at a time, like from a terminal.
	for _, key := range cfg.DetachKeys {
		if _, err := stdinWriter.Write([]byte{key}); err != nil {
			t.Fatalf("write detach key: %v", err)
		}
	}

	if err := <-attachErr; !errors.Is(err, detach.ErrDetach) {
		t.Fatalf("got error %v, want %v", err, detach.ErrDetach)
	}

	// The output stopped before the attach returned.
	out, closed := stdout.state()
	if !closed {
		t.Fatal("stdout not closed when the attach returned")
	}

	time.Sleep(50 * time.Millisecond)

	if later, _ := stdout.state(); later != out {
		t.Fatalf("output written after the attach returned: %q", later[len(out):])
	}
}

// countingOut counts how often it got closed.
type countingOut struct {
	closes atomic.Int32
}

func (*countingOut) Write(b []byte) (int, error) { return len(b), nil }

func (o *countingOut) Close() error {
	o.closes.Add(1)

	return nil
}

func TestCloseOutputStreams(t *testing.T) {
	t.Parallel()

	// None at all.
	closeOutputStreams(AttachStreams{})

	shared := &countingOut{}
	out := &Out{shared}
	closeOutputStreams(AttachStreams{Stdout: out, Stderr: out})

	if got := shared.closes.Load(); got != 1 {
		t.Fatalf("shared stream closed %d times, want once", got)
	}

	// Each stream wraps the same writer in an Out of its own, like CRI-O
	// does it.
	wrapped := &countingOut{}
	closeOutputStreams(AttachStreams{Stdout: &Out{wrapped}, Stderr: &Out{wrapped}})

	if got := wrapped.closes.Load(); got != 1 {
		t.Fatalf("wrapped stream closed %d times, want once", got)
	}

	stdout, stderr := &countingOut{}, &countingOut{}
	closeOutputStreams(AttachStreams{Stdout: &Out{stdout}, Stderr: &Out{stderr}})

	if stdout.closes.Load() != 1 || stderr.closes.Load() != 1 {
		t.Fatalf("got closes %d and %d, want one each", stdout.closes.Load(), stderr.closes.Load())
	}

	stderrOnly := &countingOut{}
	closeOutputStreams(AttachStreams{Stderr: &Out{stderrOnly}})

	if got := stderrOnly.closes.Load(); got != 1 {
		t.Fatalf("stderr closed %d times, want once", got)
	}
}

func TestForwardResizeEvents(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	requests, _ := serveRecording(t, dir)

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	// Without a channel nothing gets forwarded.
	c.forwardResizeEvents(t.Context(), &AttachConfig{ID: "id"}, nil)

	resizes := make(chan resize.TerminalSize, 3)

	// Sizes without a width or height get skipped.
	for _, size := range []resize.TerminalSize{
		{Width: 0, Height: 24},
		{Width: 80, Height: 0},
		{Width: 80, Height: 24},
	} {
		resizes <- size
	}

	close(resizes)

	// Returns once the channel got closed.
	c.forwardResizeEvents(t.Context(), &AttachConfig{ID: "id", Resize: resizes}, nil)

	want := request{
		method: "SetWindowSizeContainer",
		text:   `(id = "id", width = 80, height = 24, metadataOld = void, metadata = [])`,
	}

	if got := <-requests; got != want {
		t.Fatalf("got request %v, want %v", got, want)
	}

	select {
	case got := <-requests:
		t.Fatalf("got another request %v", got)
	default:
	}

	// Returns once the attach is done, without reading the channel.
	done := make(chan struct{})
	close(done)

	pending := make(chan resize.TerminalSize)
	c.forwardResizeEvents(t.Context(), &AttachConfig{ID: "id", Resize: pending}, done)
}
