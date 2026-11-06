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
	"syscall"
	"testing"
	"time"

	"github.com/sirupsen/logrus"
	logrustest "github.com/sirupsen/logrus/hooks/test"
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
	cfg := &AttachContainerConfig{Streams: AttachStreams{Stdout: stdout, Stderr: stderr}}

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

	cont, err := c.handlePacket(&AttachContainerConfig{}, packet, len(packet), true)
	if err != nil || !cont {
		t.Fatalf("got %v, %v for a missing stream, want to continue", cont, err)
	}

	cfg = &AttachContainerConfig{Streams: AttachStreams{Stdout: &testOut{err: io.ErrClosedPipe}}}

	if _, err := c.handlePacket(cfg, packet, len(packet), true); !errors.Is(err, io.ErrClosedPipe) {
		t.Fatalf("got error %v, want %v", err, io.ErrClosedPipe)
	}

	cfg = &AttachContainerConfig{Streams: AttachStreams{Stdout: shortOut{}}}

	if _, err := c.handlePacket(cfg, packet, len(packet), true); !errors.Is(err, io.ErrShortWrite) {
		t.Fatalf("got error %v, want %v", err, io.ErrShortWrite)
	}
}

// packetPair returns both ends of a unix packet socket pair, like the ones
// of the attach sockets. Both get closed with the test.
func packetPair(t *testing.T) (a, b *net.UnixConn) {
	t.Helper()

	// Without SOCK_CLOEXEC, which not every platform the client builds for
	// has, under the fork lock like the net package does it, so that no
	// child of a parallel test inherits the descriptors.
	syscall.ForkLock.RLock()

	pair, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_SEQPACKET, 0)
	if err == nil {
		syscall.CloseOnExec(pair[0])
		syscall.CloseOnExec(pair[1])
	}

	syscall.ForkLock.RUnlock()

	if err != nil {
		t.Fatalf("socketpair: %v", err)
	}

	unixConn := func(fd int) *net.UnixConn {
		file := os.NewFile(uintptr(fd), "packet socket")
		defer file.Close()

		conn, err := net.FileConn(file)
		if err != nil {
			t.Fatalf("file conn: %v", err)
		}

		t.Cleanup(func() { conn.Close() })

		unixConn, ok := conn.(*net.UnixConn)
		if !ok {
			t.Fatalf("got %T, want a unix connection", conn)
		}

		return unixConn
	}

	return unixConn(pair[0]), unixConn(pair[1])
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
	cfg := &AttachContainerConfig{Streams: AttachStreams{Stdout: stdout, Stderr: stderr}}

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
	cfg = &AttachContainerConfig{Streams: AttachStreams{Stdout: stdout}}

	if err := c.redirectResponseToOutputStreams(t.Context(), cfg, client); err != nil {
		t.Fatalf("redirect after close: %v", err)
	}

	if _, closed := stdout.state(); !closed {
		t.Error("stdout not closed at the end of the connection")
	}
}

// scriptedReader returns its reads in order, a packet or an error each,
// and io.EOF after the last one.
type scriptedReader struct {
	reads []scriptedRead
}

type scriptedRead struct {
	packet []byte
	err    error
}

func (r *scriptedReader) Read(b []byte) (int, error) {
	if len(r.reads) == 0 {
		return 0, io.EOF
	}

	read := r.reads[0]
	r.reads = r.reads[1:]

	return copy(b, read.packet), read.err
}

func TestRedirectResponseToOutputStreamsRetriesAndSkips(t *testing.T) {
	t.Parallel()

	reset := &net.OpError{Op: "read", Err: os.NewSyscallError("read", syscall.ECONNRESET)}

	logger, hook := logrustest.NewNullLogger()
	c := &ConmonClient{logger: logger}
	stdout := &testOut{}
	cfg := &AttachContainerConfig{Streams: AttachStreams{Stdout: stdout}}
	conn := &scriptedReader{reads: []scriptedRead{
		{packet: []byte{attachPipeStdout, 'a'}},
		// A reset connection gets read again.
		{err: reset},
		// Also with a packet which came along.
		{packet: []byte{attachPipeStdout, 'b'}, err: reset},
		// A packet of an unknown type gets skipped.
		{packet: []byte{42, 'x'}},
		{packet: []byte{attachPipeStdout, 'c'}},
	}}

	if err := c.redirectResponseToOutputStreams(t.Context(), cfg, conn); err != nil {
		t.Fatalf("redirect: %v", err)
	}

	if out, closed := stdout.state(); out != "abc" || !closed {
		t.Fatalf("got %q, closed %v, want %q, closed", out, closed, "abc")
	}

	const unexpected = "Received unexpected attach type +42"
	if entry := hook.LastEntry(); entry == nil || entry.Message != unexpected {
		t.Errorf("got log entry %v, want %q", entry, unexpected)
	}

	// Other errors end the output.
	stdout = &testOut{}
	cfg = &AttachContainerConfig{Streams: AttachStreams{Stdout: stdout}}
	conn = &scriptedReader{reads: []scriptedRead{{err: io.ErrUnexpectedEOF}}}

	if err := c.redirectResponseToOutputStreams(
		t.Context(), cfg, conn,
	); !errors.Is(err, io.ErrUnexpectedEOF) {
		t.Fatalf("got error %v, want %v", err, io.ErrUnexpectedEOF)
	}

	if _, closed := stdout.state(); !closed {
		t.Error("stdout not closed after a failed read")
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
			cfg := &AttachContainerConfig{
				ID:                "id",
				SocketPath:        serveAttachSocket(t),
				Tty:               tc.tty,
				StopAfterStdinEOF: tc.stopAfterStdinEOF,
				ContainerStdin:    tc.containerStdin,
				Streams:           AttachStreams{Stdin: stdin, Stdout: stdout},
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
			cfg := &AttachContainerConfig{
				ID:         "id",
				SocketPath: serveAttachSocket(t),
				Streams:    AttachStreams{Stdout: stdoutWriter},
			}

			if withStdin {
				stdin, _ := io.Pipe()
				cfg.Streams.Stdin = stdin
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
	cfg := &AttachContainerConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: stdin, Stdout: stdoutWriter},
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

func TestStdinErrorStopsOutput(t *testing.T) {
	t.Parallel()

	c := newTestClient(t, t.TempDir(), logrus.New())

	stdin, stdinWriter := io.Pipe()
	// A slow stream, which is still being written to on the failure.
	stdout := &testOut{delay: 20 * time.Millisecond}
	cfg := &AttachContainerConfig{
		ID:         "id",
		SocketPath: serveAttachSocket(t),
		Streams:    AttachStreams{Stdin: stdin, Stdout: stdout},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	waitFor(t, "no output", func() bool {
		out, _ := stdout.state()

		return out != ""
	})

	// Reading the stdin of the caller fails, which ends the attach.
	stdinWriter.CloseWithError(io.ErrUnexpectedEOF)

	if err := <-attachErr; !errors.Is(err, io.ErrUnexpectedEOF) {
		t.Fatalf("got error %v, want %v", err, io.ErrUnexpectedEOF)
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

func TestStdinAfterServerClosedKeepsOutput(t *testing.T) {
	t.Parallel()

	path := filepath.Join(t.TempDir(), "attach")

	listener, err := net.ListenUnix("unixpacket", &net.UnixAddr{Name: path, Net: "unixpacket"})
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	t.Cleanup(func() { listener.Close() })

	// Like after the exit of the container, the server reads no input
	// anymore, a write of the client fails with EPIPE, while the rest of
	// the output is still to come.
	sendRest := make(chan struct{})

	go func() {
		conn, err := listener.AcceptUnix()
		if err != nil {
			return
		}

		defer conn.Close()

		if err := conn.CloseRead(); err != nil {
			return
		}

		if _, err := conn.Write([]byte{attachPipeStdout, 'a'}); err != nil {
			return
		}

		<-sendRest

		_, _ = conn.Write([]byte{attachPipeStdout, 'b'}) //nolint:errcheck // checked by the output
	}()

	c := newTestClient(t, t.TempDir(), logrus.New())
	t.Cleanup(func() { c.Close() })

	stdin, stdinWriter := io.Pipe()
	stdout := &testOut{}
	cfg := &AttachContainerConfig{
		ID:         "id",
		SocketPath: path,
		Streams:    AttachStreams{Stdin: stdin, Stdout: stdout},
	}

	attachErr := make(chan error, 1)

	go func() { attachErr <- c.attach(t.Context(), cfg) }()

	waitFor(t, "no output", func() bool {
		out, _ := stdout.state()

		return out != ""
	})

	if _, err := stdinWriter.Write([]byte("x")); err != nil {
		t.Fatalf("write stdin: %v", err)
	}

	// The failed write of stdin does not end the attach.
	select {
	case err := <-attachErr:
		t.Fatalf("attach ended with %v after the write of stdin failed", err)
	case <-time.After(200 * time.Millisecond):
	}

	close(sendRest)

	select {
	case err := <-attachErr:
		if err != nil {
			t.Fatalf("attach failed: %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("attach still running after the end of the output")
	}

	if out, closed := stdout.state(); out != "ab" || !closed {
		t.Fatalf("got output %q, closed %v, want %q, closed", out, closed, "ab")
	}
}

func TestStdinPacketsBounded(t *testing.T) {
	t.Parallel()

	path := filepath.Join(t.TempDir(), "attach")

	listener, err := net.ListenUnix("unixpacket", &net.UnixAddr{Name: path, Net: "unixpacket"})
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	t.Cleanup(func() { listener.Close() })

	data := bytes.Repeat([]byte("x"), 3*attachStdinPacketSize+1)
	largest := make(chan int, 1)

	// Receives the packets with room for larger ones, and ends the output
	// once all of stdin arrived.
	go func() {
		conn, err := listener.AcceptUnix()
		if err != nil {
			return
		}

		defer conn.Close()

		buf := make([]byte, 2*len(data))
		received, maxPacket := 0, 0

		for received < len(data) {
			n, err := conn.Read(buf)
			if err != nil || n == 0 {
				break
			}

			received += n
			maxPacket = max(maxPacket, n)
		}

		largest <- maxPacket

		_, _ = conn.Write([]byte{attachPipeDone}) //nolint:errcheck // checked by the attach
	}()

	c := newTestClient(t, t.TempDir(), logrus.New())
	t.Cleanup(func() { c.Close() })

	// The reader of io.NopCloser keeps the io.WriterTo of a bytes.Reader,
	// which would write all of it at once.
	cfg := &AttachContainerConfig{
		ID:         "id",
		SocketPath: path,
		Streams: AttachStreams{
			Stdin:  io.NopCloser(bytes.NewReader(data)),
			Stdout: &testOut{},
		},
	}

	if err := c.attach(t.Context(), cfg); err != nil {
		t.Fatalf("attach: %v", err)
	}

	if got := <-largest; got > attachStdinPacketSize {
		t.Fatalf("got a stdin packet of %d bytes, want at most %d", got, attachStdinPacketSize)
	}
}

func TestStdinConnClosed(t *testing.T) {
	t.Parallel()

	for _, err := range []error{
		io.ErrClosedPipe,
		&net.OpError{Op: "write", Err: net.ErrClosed},
		&net.OpError{Op: "write", Err: os.NewSyscallError("write", syscall.EPIPE)},
		&net.OpError{Op: "write", Err: os.NewSyscallError("write", syscall.ECONNRESET)},
	} {
		if !stdinConnClosed(err) {
			t.Errorf("%v: not a closed connection", err)
		}
	}

	// Reading the stdin of the caller failed, like a connection which got
	// reset, which the attach reports.
	for _, err := range []error{
		nil, io.ErrUnexpectedEOF, net.ErrClosed, syscall.EPIPE,
		&net.OpError{Op: "read", Err: net.ErrClosed},
		&net.OpError{Op: "read", Err: os.NewSyscallError("read", syscall.ECONNRESET)},
	} {
		if stdinConnClosed(err) {
			t.Errorf("%v: a closed connection", err)
		}
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
	closeOutputStreams(AttachStreams{Stdout: shared, Stderr: shared})

	if got := shared.closes.Load(); got != 1 {
		t.Fatalf("shared stream closed %d times, want once", got)
	}

	stdout, stderr := &countingOut{}, &countingOut{}
	closeOutputStreams(AttachStreams{Stdout: stdout, Stderr: stderr})

	if stdout.closes.Load() != 1 || stderr.closes.Load() != 1 {
		t.Fatalf("got closes %d and %d, want one each", stdout.closes.Load(), stderr.closes.Load())
	}

	stderrOnly := &countingOut{}
	closeOutputStreams(AttachStreams{Stderr: stderrOnly})

	if got := stderrOnly.closes.Load(); got != 1 {
		t.Fatalf("stderr closed %d times, want once", got)
	}
}

// sliceOut is a writer whose type is not comparable.
type sliceOut []byte

func (sliceOut) Write(p []byte) (int, error) { return len(p), nil }

func (sliceOut) Close() error { return nil }

func TestSameWriter(t *testing.T) {
	t.Parallel()

	w := &countingOut{}

	if !sameWriter(w, w) {
		t.Error("same writer not detected")
	}

	if sameWriter(w, &countingOut{}) {
		t.Error("different writers detected as the same")
	}

	// Compared without the guard, they would panic.
	if sameWriter(sliceOut{}, sliceOut{}) {
		t.Error("writers which are not comparable detected as the same")
	}

	closeOutputStreams(AttachStreams{Stdout: sliceOut{}, Stderr: sliceOut{}})
}

func TestForwardResizeEvents(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	requests, _ := serveRecording(t, dir)

	c := newTestClient(t, dir, logrus.New())
	t.Cleanup(func() { c.Close() })

	// Without a channel nothing gets forwarded.
	c.forwardResizeEvents(t.Context(), &AttachContainerConfig{ID: "id"}, nil)

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
	c.forwardResizeEvents(t.Context(), &AttachContainerConfig{ID: "id", Resize: resizes}, nil)

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
	c.forwardResizeEvents(t.Context(), &AttachContainerConfig{ID: "id", Resize: pending}, done)
}
