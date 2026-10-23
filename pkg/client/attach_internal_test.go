package client

import (
	"bytes"
	"errors"
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
		cont, err := c.handlePacket(t.Context(), cfg, tc.packet, len(tc.packet))
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

	cont, err := c.handlePacket(t.Context(), &AttachConfig{}, packet, len(packet))
	if err != nil || !cont {
		t.Fatalf("got %v, %v for a missing stream, want to continue", cont, err)
	}

	cfg = &AttachConfig{Streams: AttachStreams{Stdout: &Out{&testOut{err: io.ErrClosedPipe}}}}

	if _, err := c.handlePacket(
		t.Context(),
		cfg,
		packet,
		len(packet),
	); !errors.Is(
		err,
		io.ErrClosedPipe,
	) {
		t.Fatalf("got error %v, want %v", err, io.ErrClosedPipe)
	}

	cfg = &AttachConfig{Streams: AttachStreams{Stdout: &Out{shortOut{}}}}

	if _, err := c.handlePacket(
		t.Context(),
		cfg,
		packet,
		len(packet),
	); !errors.Is(
		err,
		io.ErrShortWrite,
	) {
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
