package client

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"sync"
	"testing"
	"time"

	"github.com/sirupsen/logrus"
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
