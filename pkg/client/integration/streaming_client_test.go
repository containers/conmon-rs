package client_test

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	neturl "net/url"
	"strconv"
	"strings"
	"time"

	"github.com/gorilla/websocket"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"golang.org/x/sys/unix"
	"k8s.io/apimachinery/pkg/util/proxy"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/portforward"
	"k8s.io/client-go/tools/remotecommand"
	"k8s.io/client-go/transport/spdy"
	"k8s.io/streaming/pkg/httpstream"
)

var (
	errNotTCP   = errors.New("not a TCP connection")
	errProtocol = errors.New("unexpected port forward protocol")
)

// closedPort returns a local port which refuses connections. Unlike the
// port of a closed listener, nothing else can take it while the spec runs: a
// socket stays bound to it without listening until the spec ends.
func closedPort() int {
	fd, err := unix.Socket(unix.AF_INET, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	Expect(err).To(Succeed())
	DeferCleanup(unix.Close, fd)

	Expect(unix.Bind(fd, &unix.SockaddrInet4{Addr: [4]byte{127, 0, 0, 1}})).To(Succeed())

	addr, err := unix.Getsockname(fd)
	Expect(err).To(Succeed())

	inet4, ok := addr.(*unix.SockaddrInet4)
	Expect(ok).To(BeTrue())

	return inet4.Port
}

// sizeQueue hands the terminal sizes sent to it to a streaming client, until
// it gets closed.
type sizeQueue chan remotecommand.TerminalSize

// terminalSizes returns a closed queue with the sizes.
func terminalSizes(sizes ...remotecommand.TerminalSize) sizeQueue {
	queue := make(sizeQueue, len(sizes))
	for _, size := range sizes {
		queue <- size
	}

	close(queue)

	return queue
}

func (q sizeQueue) Next() *remotecommand.TerminalSize {
	size, ok := <-q
	if !ok {
		return nil
	}

	return &size
}

// streamTransports are the transports of exec and attach sessions: WebSocket
// with v5.channel.k8s.io like kubectl, and SPDY like the kubelet towards the
// runtime.
var streamTransports = []string{"WebSocket", "SPDY"}

// streamURLOver runs the exec or attach session served at url over the
// transport until it ends.
func streamURLOver(
	ctx context.Context,
	transport, rawURL string,
	options remotecommand.StreamOptions,
) error {
	var (
		executor remotecommand.Executor
		err      error
	)

	if transport == "SPDY" {
		parsed, parseErr := neturl.Parse(rawURL)
		Expect(parseErr).NotTo(HaveOccurred())

		executor, err = remotecommand.NewSPDYExecutor(&rest.Config{}, http.MethodPost, parsed)
	} else {
		executor, err = remotecommand.NewWebSocketExecutor(
			&rest.Config{Insecure: true},
			http.MethodGet,
			rawURL,
		)
	}

	Expect(err).NotTo(HaveOccurred())

	if err := executor.StreamWithContext(
		ctx,
		options,
	); err != nil {
		return fmt.Errorf("create stream with context: %w", err)
	}

	return nil
}

// legacyExec runs the exec session served at url with a WebSocket protocol
// before v5, which has no close signal for stdin. It returns what the client
// received on stdout, stderr and the error channel until the server closed
// the session.
func legacyExec(rawURL, protocol string) (stdout, stderr, status string, err error) {
	base64Codec := strings.Contains(protocol, "base64")
	wsURL := strings.Replace(rawURL, "http://", "ws://", 1)
	dialer := websocket.Dialer{Subprotocols: []string{protocol}, HandshakeTimeout: 5 * time.Second}

	conn, resp, err := dialer.Dial(wsURL, nil)
	if resp != nil {
		resp.Body.Close()
	}

	if err != nil {
		return "", "", "", fmt.Errorf("dial: %w", err)
	}

	defer conn.Close()

	if conn.Subprotocol() != protocol {
		return "", "", "", fmt.Errorf("%w: %q", errProtocol, conn.Subprotocol())
	}

	received := map[byte]string{}

	for {
		if err := conn.SetReadDeadline(time.Now().Add(10 * time.Second)); err != nil {
			return "", "", "", fmt.Errorf("set deadline: %w", err)
		}

		_, message, err := conn.ReadMessage()
		if websocket.IsCloseError(err, websocket.CloseNormalClosure) {
			break
		}

		if err != nil {
			return "", "", "", fmt.Errorf("read message: %w", err)
		}

		channel, content := message[0], message[1:]
		if base64Codec {
			channel -= '0'

			if content, err = base64.StdEncoding.DecodeString(string(content)); err != nil {
				return "", "", "", fmt.Errorf("decode message: %w", err)
			}
		}

		received[channel] += string(content)
	}

	return received[1], received[2], received[3], nil
}

// portForwardDialer dials the port forward session served at url over SPDY,
// like the kubelet does.
//
//nolint:ireturn // client-go only exposes the interface
func portForwardDialer(rawURL string) (httpstream.Dialer, error) {
	parsed, err := neturl.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("parse url: %w", err)
	}

	transport, upgrader, err := spdy.RoundTripperFor(&rest.Config{})
	if err != nil {
		return nil, fmt.Errorf("create round tripper: %w", err)
	}

	return spdy.NewDialerForStreaming(
		upgrader,
		&http.Client{Transport: transport},
		http.MethodPost,
		parsed,
	), nil
}

// tunnelDialer dials the port forward session served at url with SPDY
// tunneled through a WebSocket, like crictl and critest do with the websocket
// transport.
//
//nolint:ireturn // client-go only exposes the interface
func tunnelDialer(rawURL string) (httpstream.Dialer, error) {
	parsed, err := neturl.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("parse url: %w", err)
	}

	dialer, err := portforward.NewSPDYOverWebsocketDialerForStreaming(parsed, &rest.Config{})
	if err != nil {
		return nil, fmt.Errorf("create tunneling dialer: %w", err)
	}

	return dialer, nil
}

// startPortForward forwards a free local port to the port of the session the
// dialer dials, with the port forwarder of client-go. The stop function ends
// the forwarding and waits for it.
func startPortForward(dialer httpstream.Dialer, port int) (local uint16, stop func(), err error) {
	stopChan := make(chan struct{})
	readyChan := make(chan struct{})

	forwarder, err := portforward.NewOnAddressesForStreaming(
		dialer, []string{"127.0.0.1"}, []string{fmt.Sprintf("0:%d", port)},
		stopChan, readyChan, GinkgoWriter, GinkgoWriter,
	)
	if err != nil {
		return 0, nil, fmt.Errorf("create port forwarder: %w", err)
	}

	errChan := make(chan error, 1)

	go func() { errChan <- forwarder.ForwardPorts() }()

	select {
	case <-readyChan:
	case forwardErr := <-errChan:
		return 0, nil, fmt.Errorf("forward ports: %w", forwardErr)
	}

	stop = func() {
		close(stopChan)

		if forwardErr := <-errChan; forwardErr != nil {
			GinkgoWriter.Printf("Port forwarding ended: %v\n", forwardErr)
		}
	}

	ports, err := forwarder.GetPorts()
	if err != nil || len(ports) != 1 {
		stop()

		return 0, nil, fmt.Errorf("get forwarded ports %v: %w", ports, err)
	}

	return ports[0].Local, stop, nil
}

// portForwardRoundTrip sends the payload to the forwarded local port, closes
// the sending side and returns everything that came back until the remote
// side closed the connection.
func portForwardRoundTrip(local uint16, payload string) (string, error) {
	dialer := net.Dialer{Timeout: 5 * time.Second}

	conn, err := dialer.DialContext(context.Background(), "tcp", fmt.Sprintf("127.0.0.1:%d", local))
	if err != nil {
		return "", fmt.Errorf("dial forwarded port: %w", err)
	}
	defer conn.Close()

	if err = conn.SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		return "", fmt.Errorf("set deadline: %w", err)
	}

	if _, err = conn.Write([]byte(payload)); err != nil {
		return "", fmt.Errorf("write payload: %w", err)
	}

	tcpConn, ok := conn.(*net.TCPConn)
	if !ok {
		return "", fmt.Errorf("%w: %T", errNotTCP, conn)
	}

	if err = tcpConn.CloseWrite(); err != nil {
		return "", fmt.Errorf("close sending side: %w", err)
	}

	response, err := io.ReadAll(conn)
	if err != nil {
		return "", fmt.Errorf("read response: %w", err)
	}

	return string(response), nil
}

// echoServer listens on a free local port and echoes every connection. With
// a size of 0 until the client closes its sending side, otherwise the size
// in bytes, then it closes the connection itself. The channel protocol cannot
// close the sending side, so its server has to end the connection.
func echoServer(size int64) (listener net.Listener, port int) {
	listener, err := (&net.ListenConfig{}).Listen(context.Background(), "tcp4", "127.0.0.1:0")
	Expect(err).NotTo(HaveOccurred())

	addr, ok := listener.Addr().(*net.TCPAddr)
	Expect(ok).To(BeTrue())

	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}

			go func() {
				defer conn.Close()

				var err error
				if size > 0 {
					_, err = io.CopyN(conn, conn, size)
				} else {
					_, err = io.Copy(conn, conn)
				}

				if err != nil {
					GinkgoWriter.Printf("Echo failed: %v\n", err)
				}
			}()
		}
	}()

	return listener, addr.Port
}

// channelRoundTrip forwards the port over the WebSocket channel protocol,
// sends the payload on the data channel and returns what the data channel
// received until the server closed the session, and the error channel.
func channelRoundTrip(
	rawURL string,
	port int,
	base64Codec bool,
	payload string,
) (data, errs string, err error) {
	wsURL := strings.Replace(rawURL, "http://", "ws://", 1) + "?port=" + strconv.Itoa(port)

	protocol, messageType := "v4.channel.k8s.io", websocket.BinaryMessage
	if base64Codec {
		protocol, messageType = "v4.base64.channel.k8s.io", websocket.TextMessage
	}

	dialer := websocket.Dialer{Subprotocols: []string{protocol}, HandshakeTimeout: 5 * time.Second}

	conn, resp, err := dialer.Dial(wsURL, nil)
	if resp != nil {
		resp.Body.Close()
	}

	if err != nil {
		return "", "", fmt.Errorf("dial: %w", err)
	}

	defer conn.Close()

	if conn.Subprotocol() != protocol {
		return "", "", fmt.Errorf("%w: %q", errProtocol, conn.Subprotocol())
	}

	encode := func(channel byte, data []byte) []byte {
		if base64Codec {
			return append([]byte{'0' + channel}, base64.StdEncoding.EncodeToString(data)...)
		}

		return append([]byte{channel}, data...)
	}

	if err := conn.WriteMessage(messageType, encode(0, []byte(payload))); err != nil {
		return "", "", fmt.Errorf("send payload: %w", err)
	}

	received := map[byte][]byte{}

	for {
		if err := conn.SetReadDeadline(time.Now().Add(10 * time.Second)); err != nil {
			return "", "", fmt.Errorf("set deadline: %w", err)
		}

		_, message, err := conn.ReadMessage()
		if websocket.IsCloseError(err, websocket.CloseNormalClosure) {
			break
		}

		if err != nil {
			return "", "", fmt.Errorf("read message: %w", err)
		}

		channel, content := message[0], message[1:]
		if base64Codec {
			channel -= '0'

			if content, err = base64.StdEncoding.DecodeString(string(content)); err != nil {
				return "", "", fmt.Errorf("decode message: %w", err)
			}
		}

		received[channel] = append(received[channel], content...)
	}

	// Both channels start with the port.
	prefix := string([]byte{byte(port), byte(port >> 8)})
	for channel := range byte(2) {
		if !strings.HasPrefix(string(received[channel]), prefix) {
			return "", "", fmt.Errorf(
				"%w: channel %d got %q",
				errProtocol,
				channel,
				received[channel],
			)
		}
	}

	return string(received[0][2:]), string(received[1][2:]), nil
}

// kubeletProxy serves the URL through the proxy the kubelet puts in front of
// the streaming server of the runtime, which forwards SPDY upgrades as they
// are. It returns the URL of the proxy and a function to stop it.
func kubeletProxy(rawURL string) (proxyURL string, stop func()) {
	location, err := neturl.Parse(rawURL)
	Expect(err).NotTo(HaveOccurred())

	server := httptest.NewServer(
		proxy.NewUpgradeAwareHandler(location, nil, false, true, proxyResponder{}),
	)

	return server.URL + location.Path, server.Close
}

// proxyResponder reports errors of the kubelet proxy, like the kubelet does.
type proxyResponder struct{}

func (proxyResponder) Error(w http.ResponseWriter, _ *http.Request, err error) {
	http.Error(w, err.Error(), http.StatusInternalServerError)
}
