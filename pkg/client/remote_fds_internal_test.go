package client

import (
	"context"
	"encoding/binary"
	"errors"
	"net"
	"os"
	"slices"
	"syscall"
	"testing"
	"time"
)

// fdServerReply builds the response of the fake fd server to a request.
type fdServerReply func(reqID uint32, fds []int) []byte

// slotsReply answers with one slot per received descriptor.
func slotsReply(reqID uint32, fds []int) []byte {
	b := binary.LittleEndian.AppendUint64(nil, uint64(reqID)<<numFDsBits|uint64(len(fds)))
	for i := range fds {
		b = binary.LittleEndian.AppendUint64(b, uint64(100+i))
	}

	return b
}

// newTestRemoteFDs returns RemoteFDs connected to a fake fd server which
// answers the requests in order with the replies. A nil reply answers
// nothing.
func newTestRemoteFDs(t *testing.T, replies ...fdServerReply) *RemoteFDs {
	t.Helper()

	client, server := packetPair(t)

	done := make(chan struct{})

	go func() {
		defer close(done)

		for _, reply := range replies {
			buf := make([]byte, uint64Bytes)
			oob := make([]byte, syscall.CmsgSpace(maxFDs*4))

			_, oobn, _, _, err := server.ReadMsgUnix(buf, oob)
			if err != nil {
				return
			}

			msgs, err := syscall.ParseSocketControlMessage(oob[:oobn])
			if err != nil || len(msgs) == 0 {
				return
			}

			fds, err := syscall.ParseUnixRights(&msgs[0])
			if err != nil {
				return
			}

			for _, fd := range fds {
				syscall.Close(fd)
			}

			if reply == nil {
				continue
			}

			reqID := uint32(binary.LittleEndian.Uint64(buf) >> numFDsBits)
			if _, err := server.Write(reply(reqID, fds)); err != nil {
				return
			}
		}
	}()

	t.Cleanup(func() {
		client.Close()
		server.Close()
		<-done
	})

	return &RemoteFDs{conn: client}
}

// packetPair returns both ends of a unix packet socket pair, like the ones
// of the fd and attach sockets. Both get closed with the test.
func packetPair(t *testing.T) (a, b *net.UnixConn) {
	t.Helper()

	pair, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_SEQPACKET|syscall.SOCK_CLOEXEC, 0)
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

// testFD returns a descriptor to send, closed with the test.
func testFD(t *testing.T) int {
	t.Helper()

	file, err := os.Open(os.DevNull)
	if err != nil {
		t.Fatalf("open: %v", err)
	}

	t.Cleanup(func() { file.Close() })

	return int(file.Fd())
}

func TestRemoteFDsSend(t *testing.T) {
	t.Parallel()

	const requests = 2

	r := newTestRemoteFDs(t, slices.Repeat([]fdServerReply{slotsReply}, requests)...)

	for range requests {
		slots, err := r.Send(testFD(t), testFD(t))
		if err != nil {
			t.Fatalf("send: %v", err)
		}

		if len(slots) != 2 || slots[0] != 100 || slots[1] != 101 {
			t.Fatalf("got slots %v, want [100 101]", slots)
		}
	}

	if err := r.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
}

func TestRemoteFDsServerErrorKeepsConnection(t *testing.T) {
	t.Parallel()

	serverErr := func(reqID uint32, _ []int) []byte {
		b := binary.LittleEndian.AppendUint64(nil, uint64(reqID)<<numFDsBits|(1<<numFDsBits-1))

		return append(b, "no slots left"...)
	}

	r := newTestRemoteFDs(t, serverErr, slotsReply)

	_, err := r.Send(testFD(t))
	if !errors.As(err, new(serverError)) {
		t.Fatalf("got error %v, want a server error", err)
	}

	if _, err := r.Send(testFD(t)); err != nil {
		t.Fatalf("send after a server error: %v", err)
	}
}

func TestRemoteFDsFailedSendKeepsConnection(t *testing.T) {
	t.Parallel()

	r := newTestRemoteFDs(t, slotsReply)

	// The kernel rejects the invalid descriptor, nothing gets sent.
	if _, err := r.Send(-1); !errors.Is(err, syscall.EBADF) {
		t.Fatalf("got error %v, want %v", err, syscall.EBADF)
	}

	if _, err := r.Send(testFD(t)); err != nil {
		t.Fatalf("send after a failed send: %v", err)
	}
}

func TestRemoteFDsInvalidResponseBreaksConnection(t *testing.T) {
	t.Parallel()

	for name, tc := range map[string]struct {
		reply fdServerReply
		want  error
	}{
		"wrong ID": {
			reply: func(reqID uint32, fds []int) []byte { return slotsReply(reqID+1, fds) },
			want:  errResponseIDDoesNotMatch,
		},
		"too short": {
			reply: func(uint32, []int) []byte { return []byte{1} },
			want:  errResponseTooShort,
		},
		"wrong number of FDs": {
			reply: func(reqID uint32, fds []int) []byte { return slotsReply(reqID, append(fds, 0)) },
			want:  errNumberOfFDsDoesNotMatch,
		},
		"truncated": {
			reply: func(reqID uint32, fds []int) []byte { return slotsReply(reqID, fds)[:12] },
			want:  errInvalidResponseLength,
		},
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			r := newTestRemoteFDs(t, tc.reply, slotsReply)

			if _, err := r.Send(testFD(t)); !errors.Is(err, tc.want) {
				t.Fatalf("got error %v, want %v", err, tc.want)
			}

			if _, err := r.Send(testFD(t)); !errors.Is(err, errConnectionBroken) {
				t.Fatalf("got error %v, want %v", err, errConnectionBroken)
			}

			if err := r.Close(); err != nil {
				t.Fatalf("close: %v", err)
			}
		})
	}
}

func TestRemoteFDsInterruptedRequestBreaksConnection(t *testing.T) {
	t.Parallel()

	// The server never answers the first request.
	r := newTestRemoteFDs(t, nil, slotsReply)

	ctx, cancel := context.WithTimeout(t.Context(), 100*time.Millisecond)
	defer cancel()

	if _, err := r.SendContext(ctx, testFD(t)); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("got error %v, want %v", err, context.DeadlineExceeded)
	}

	if _, err := r.Send(testFD(t)); !errors.Is(err, errConnectionBroken) {
		t.Fatalf("got error %v, want %v", err, errConnectionBroken)
	}

	if err := r.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
}
