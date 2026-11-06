package client_test

import (
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"go.podman.io/storage/pkg/unshare"
	"k8s.io/client-go/tools/portforward"
)

var _ = Describe("StreamingServer", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)
	newServer := serverStarter(&tr, &sut)

	Describe("ServePortForwardContainer", func() {
		It("should forward concurrent connections", func(ctx SpecContext) {
			newServer()

			listener, port := echoServer(0)
			defer listener.Close()

			// The server runs in the network namespace of the test, which
			// needs no privileges to connect in.
			result, err := sut.ServePortForwardContainer(
				ctx,
				&client.ServePortForwardContainerConfig{
					NetNsPath:   "/proc/self/ns/net",
					IdleTimeout: time.Minute,
				},
			)
			Expect(err).NotTo(HaveOccurred())
			Expect(result.URL).To(ContainSubstring("/portforward/"))

			// The streaming server got started.
			resources := resourcesOf(sut.PID())

			dialer, err := portForwardDialer(result.URL)
			Expect(err).NotTo(HaveOccurred())

			local, stopForward, err := startPortForward(dialer, port)
			Expect(err).NotTo(HaveOccurred())

			stop := sync.OnceFunc(stopForward)
			defer stop()

			// All connections share the SPDY connection of the forwarder.
			var wg sync.WaitGroup
			for i := range 5 {
				wg.Go(func() {
					defer GinkgoRecover()

					payload := strings.Repeat(strconv.Itoa(i), 100*1024)
					Expect(portForwardRoundTrip(local, payload)).To(Equal(payload))
				})
			}

			wg.Wait()

			// The session ends with the forwarder, its namespace thread
			// and connections with it.
			stop()
			expectNoResourceLeak(sut.PID(), resources)
		}, SpecTimeout(specTimeout))

		It("should fail requests beyond the limit of waiting URLs", func(ctx SpecContext) {
			newServer()

			// The server only checks that the path is absolute before it
			// returns the URL, it enters the namespace once a client
			// connected, which none does here.
			cfg := &client.ServePortForwardContainerConfig{NetNsPath: "/proc/self/ns/net"}

			const waitingURLs = 1000
			for range waitingURLs {
				_, err := sut.ServePortForwardContainer(ctx, cfg)
				Expect(err).NotTo(HaveOccurred())
			}

			_, err := sut.ServePortForwardContainer(ctx, cfg)
			Expect(err).To(MatchError(client.ErrTooManyRequests))
		}, SpecTimeout(specTimeout))

		It("should forward connections through a WebSocket tunnel", func(ctx SpecContext) {
			newServer()

			listener, port := echoServer(0)
			defer listener.Close()

			result, err := sut.ServePortForwardContainer(
				ctx,
				&client.ServePortForwardContainerConfig{
					NetNsPath: "/proc/self/ns/net",
				},
			)
			Expect(err).NotTo(HaveOccurred())

			dialer, err := tunnelDialer(result.URL)
			Expect(err).NotTo(HaveOccurred())

			local, stop, err := startPortForward(dialer, port)
			Expect(err).NotTo(HaveOccurred())

			defer stop()

			var wg sync.WaitGroup
			for i := range 3 {
				wg.Go(func() {
					defer GinkgoRecover()

					payload := strings.Repeat(strconv.Itoa(i), 100*1024)
					Expect(portForwardRoundTrip(local, payload)).To(Equal(payload))
				})
			}

			wg.Wait()
		}, SpecTimeout(specTimeout))

		for _, base64Codec := range []bool{false, true} {
			It(
				fmt.Sprintf(
					"should forward a port over the WebSocket channel protocol (base64: %v)",
					base64Codec,
				),
				func(ctx SpecContext) {
					newServer()

					payload := strings.Repeat("x", 100*1024)
					listener, port := echoServer(int64(len(payload)))

					defer listener.Close()

					result, err := sut.ServePortForwardContainer(
						ctx,
						&client.ServePortForwardContainerConfig{
							NetNsPath: "/proc/self/ns/net",
						},
					)
					Expect(err).NotTo(HaveOccurred())

					data, errs, err := channelRoundTrip(result.URL, port, base64Codec, payload)
					Expect(err).NotTo(HaveOccurred())
					Expect(errs).To(BeEmpty())
					Expect(data).To(Equal(payload))
				}, SpecTimeout(specTimeout),
			)
		}

		It("should report a closed port on the WebSocket error channel", func(ctx SpecContext) {
			newServer()

			port := closedPort()

			result, err := sut.ServePortForwardContainer(
				ctx,
				&client.ServePortForwardContainerConfig{
					NetNsPath: "/proc/self/ns/net",
				},
			)
			Expect(err).NotTo(HaveOccurred())

			data, errs, err := channelRoundTrip(result.URL, port, false, "request")
			Expect(err).NotTo(HaveOccurred())
			Expect(data).To(BeEmpty())
			Expect(errs).To(ContainSubstring("failed to connect to localhost:%d", port))
		}, SpecTimeout(specTimeout))

		It("should report a closed port on the error stream", func(ctx SpecContext) {
			newServer()

			port := closedPort()

			result, err := sut.ServePortForwardContainer(
				ctx,
				&client.ServePortForwardContainerConfig{
					NetNsPath: "/proc/self/ns/net",
				},
			)
			Expect(err).NotTo(HaveOccurred())

			dialer, err := portForwardDialer(result.URL)
			Expect(err).NotTo(HaveOccurred())
			conn, protocol, err := dialer.Dial(portforward.PortForwardProtocolV1Name)
			Expect(err).NotTo(HaveOccurred())

			defer conn.Close()

			Expect(protocol).To(Equal(portforward.PortForwardProtocolV1Name))

			// The streams of a request, like client-go opens them. SPDY
			// sends the header names in lowercase.
			headers := http.Header{
				"Streamtype": {"error"},
				"Port":       {strconv.Itoa(port)},
				"Requestid":  {"0"},
			}
			errorStream, err := conn.CreateStream(headers)
			Expect(err).NotTo(HaveOccurred())
			Expect(errorStream.Close()).To(Succeed())

			headers.Set("Streamtype", "data")
			_, err = conn.CreateStream(headers)
			Expect(err).NotTo(HaveOccurred())

			message, err := io.ReadAll(errorStream)
			Expect(err).NotTo(HaveOccurred())
			Expect(string(message)).To(ContainSubstring(
				"error forwarding port %d: failed to connect to localhost:%d", port, port,
			))
		}, SpecTimeout(specTimeout))

		It(
			"should forward a port into the network namespace of a container",
			func(ctx SpecContext) {
				if unshare.IsRootless() {
					Skip("does not run rootless")
				}

				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					false,
					[]string{"/busybox", "nc", "-lk", "-p", "8080", "-e", "/busybox", "cat"},
					nil,
				)
				sut = tr.configGivenEnv()
				resp, err := sut.CreateContainer(ctx, tr.defaultConfig(false))
				Expect(err).NotTo(HaveOccurred())
				tr.startContainer()

				netNsPath := fmt.Sprintf("/proc/%d/ns/net", resp.PID)
				ownNetNs, err := os.Readlink("/proc/self/ns/net")
				Expect(err).NotTo(HaveOccurred())
				Expect(os.Readlink(netNsPath)).NotTo(Equal(ownNetNs))

				// Until nc listens. A failed request ends the forwarder, so each
				// attempt starts a new one.
				Eventually(func() (string, error) {
					result, err := sut.ServePortForwardContainer(
						ctx,
						&client.ServePortForwardContainerConfig{
							NetNsPath: netNsPath,
						},
					)
					if err != nil {
						return "", fmt.Errorf("serve port forward: %w", err)
					}

					dialer, err := portForwardDialer(result.URL)
					if err != nil {
						return "", err
					}

					local, stop, err := startPortForward(dialer, 8080)
					if err != nil {
						return "", err
					}
					defer stop()

					return portForwardRoundTrip(local, "hello")
				}, 10*time.Second).Should(Equal("hello"))
			},
			SpecTimeout(specTimeout),
		)
	})
})
