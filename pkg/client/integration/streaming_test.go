package client_test

import (
	"bytes"
	"context"
	"errors"
	"io"
	"strings"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"k8s.io/client-go/tools/remotecommand"
	utilexec "k8s.io/client-go/util/exec"
)

var _ = Describe("StreamingServer", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)
	newServer := serverStarter(&tr, &sut)
	runContainer := containerRunner(&tr, &sut)

	for _, transport := range streamTransports {
		Describe("ServeExecContainer over "+transport, func() {
			for _, terminal := range []bool{true, false} {
				It(testName("should succeed", terminal), func(ctx SpecContext) {
					// Runs past the wait for the leak check, its exit would
					// free resources.
					runContainer(ctx, terminal, []string{"/busybox", "sleep", "600"})

					result, err := sut.ServeExecContainer(
						ctx,
						&client.ServeExecContainerConfig{
							ID: tr.ctrID,
							Command: []string{
								"/busybox",
								"sh",
								"-c",
								"echo -n stdout && echo -n stderr >&2",
							},
							Tty:    terminal,
							Stdout: true,
							Stderr: true,
						},
					)

					Expect(err).To(Succeed())
					Expect(result.URL).NotTo(BeEmpty())

					// The streaming server and the session got set up.
					resources := resourcesOf(sut.PID())

					stdout := &bytes.Buffer{}
					stderr := &bytes.Buffer{}

					Expect(
						streamURLOver(
							ctx,
							transport,
							result.URL,
							remotecommand.StreamOptions{
								Stdout: stdout,
								Stderr: stderr,
								Tty:    terminal,
							},
						),
					).To(Succeed())

					if terminal {
						Expect(stdout.String()).To(Equal("stdoutstderr"))
						Expect(stderr.Len()).To(BeZero())
					} else {
						Expect(stdout.String()).To(Equal("stdout"))
						Expect(stderr.String()).To(Equal("stderr"))
					}

					// The PID file of the exec is removed once it got read.
					Expect(execPIDFiles(tr.tmpDir)).To(BeEmpty())

					expectNoResourceLeak(sut.PID(), resources)
				}, SpecTimeout(specTimeout))

				if terminal {
					It("should apply resize events", func(ctx SpecContext) {
						runContainer(ctx, terminal, []string{"/busybox", "sleep", "600"})

						result, err := sut.ServeExecContainer(
							ctx,
							&client.ServeExecContainerConfig{
								ID:      tr.ctrID,
								Command: []string{"/busybox", "sh", "-c", sttyAwait},
								Tty:     terminal,
								Stdout:  true,
							},
						)
						Expect(err).To(Succeed())

						sizes := terminalSizes(remotecommand.TerminalSize{Width: 30, Height: 40})
						stdout := &bytes.Buffer{}

						Expect(
							streamURLOver(ctx, transport, result.URL, remotecommand.StreamOptions{
								Stdout:            stdout,
								Tty:               terminal,
								TerminalSizeQueue: sizes,
							}),
						).To(Succeed())
						Expect(stdout.String()).To(ContainSubstring("40 30"))
					}, SpecTimeout(specTimeout))
				}

				It(testName("should report a non-zero exit code", terminal), func(ctx SpecContext) {
					runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

					result, err := sut.ServeExecContainer(
						ctx,
						&client.ServeExecContainerConfig{
							ID:      tr.ctrID,
							Command: []string{"/busybox", "sh", "-c", "exit 3"},
							Tty:     terminal,
							Stdout:  true,
							Stderr:  true,
						},
					)
					Expect(err).To(Succeed())

					err = streamURLOver(
						ctx,
						transport,
						result.URL,
						remotecommand.StreamOptions{
							Stdout: io.Discard,
							Stderr: io.Discard,
							Tty:    terminal,
						},
					)

					var exitErr utilexec.CodeExitError
					Expect(errors.As(err, &exitErr)).To(BeTrue(), "error %v", err)
					Expect(exitErr.Code).To(Equal(3))
				}, SpecTimeout(specTimeout))

				It(
					testName("should not keep the PID file of a failed exec", terminal),
					func(ctx SpecContext) {
						runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

						result, err := sut.ServeExecContainer(
							ctx,
							&client.ServeExecContainerConfig{
								ID:      tr.ctrID,
								Command: []string{"/does/not/exist"},
								Tty:     terminal,
								Stdout:  true,
								Stderr:  true,
							},
						)
						Expect(err).To(Succeed())

						// The runtime fails to start the command, the session reports it.
						Expect(
							streamURLOver(
								ctx,
								transport,
								result.URL,
								remotecommand.StreamOptions{
									Stdout: io.Discard,
									Stderr: io.Discard,
									Tty:    terminal,
								},
							),
						).NotTo(Succeed())

						Eventually(func() []string { return execPIDFiles(tr.tmpDir) }).
							WithTimeout(10 * time.Second).
							Should(BeEmpty())

						// The helper the runtime left in the PID namespace of the
						// container is reaped, otherwise the container could not stop.
						Eventually(func() []int { return zombieChildrenOf(int(sut.PID())) }).
							WithTimeout(10 * time.Second).
							Should(BeEmpty())
					},
					SpecTimeout(specTimeout),
				)
			}

			It("should receive the output written after stdin EOF", func(ctx SpecContext) {
				runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

				result, err := sut.ServeExecContainer(
					ctx,
					&client.ServeExecContainerConfig{
						ID: tr.ctrID,
						Command: []string{
							"/busybox",
							"sh",
							"-c",
							"/busybox cat && echo stdin closed",
						},
						Stdin:  true,
						Stdout: true,
						Stderr: true,
					},
				)
				Expect(err).To(Succeed())

				stdout := &bytes.Buffer{}
				stderr := &bytes.Buffer{}

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				Expect(streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
					Stdin:  strings.NewReader("attached"),
					Stdout: stdout,
					Stderr: stderr,
				})).To(Succeed())
				Expect(stdout.String()).To(Equal("attachedstdin closed\n"))
				Expect(stderr.String()).To(BeEmpty())
			}, SpecTimeout(specTimeout))

			It("should keep an interactive stdin open", func(ctx SpecContext) {
				runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

				result, err := sut.ServeExecContainer(
					ctx,
					&client.ServeExecContainerConfig{
						ID: tr.ctrID,
						Command: []string{
							"/busybox",
							"sh",
							"-c",
							"read -r line; echo \"got $line\"",
						},
						Stdin:  true,
						Stdout: true,
						Stderr: true,
					},
				)
				Expect(err).To(Succeed())

				// The line comes after the session started, stdin stays open
				// meanwhile.
				stdinReader, stdinWriter := io.Pipe()

				go func() {
					defer GinkgoRecover()
					defer stdinWriter.Close()

					time.Sleep(500 * time.Millisecond)

					_, err := stdinWriter.Write([]byte("later\n"))
					Expect(err).To(Succeed())
				}()

				stdout := &bytes.Buffer{}

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				Expect(streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
					Stdin:  stdinReader,
					Stdout: stdout,
					Stderr: io.Discard,
				})).To(Succeed())
				Expect(stdout.String()).To(Equal("got later\n"))
			}, SpecTimeout(specTimeout))

			It("should end the stdin of a terminal", func(ctx SpecContext) {
				runContainer(ctx, true, []string{"/busybox", "sleep", "600"})

				// cat only ends with the end of file character, which the
				// server writes to the terminal once stdin ended.
				result, err := sut.ServeExecContainer(
					ctx,
					&client.ServeExecContainerConfig{
						ID: tr.ctrID,
						Command: []string{
							"/busybox",
							"sh",
							"-c",
							"read -r l; echo \"got $l\"; /busybox cat >/dev/null; echo eof",
						},
						Tty:    true,
						Stdin:  true,
						Stdout: true,
					},
				)
				Expect(err).To(Succeed())

				stdout := &bytes.Buffer{}

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				Expect(streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
					Stdin:  strings.NewReader("x\n"),
					Stdout: stdout,
					Tty:    true,
				})).To(Succeed())
				Expect(stdout.String()).To(And(
					ContainSubstring("got x\r\n"),
					HaveSuffix("eof\r\n"),
				))
			}, SpecTimeout(specTimeout))

			It("should close an idle session", func(ctx SpecContext) {
				runContainer(ctx, false, []string{"/busybox", "sleep", "600"})

				// A command no other process runs, to find the exec process.
				command := []string{"/busybox", "sleep", "317"}

				result, err := sut.ServeExecContainer(
					ctx,
					&client.ServeExecContainerConfig{
						ID:          tr.ctrID,
						Command:     command,
						Stdout:      true,
						Stderr:      true,
						IdleTimeout: 2 * time.Second,
					},
				)
				Expect(err).To(Succeed())

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				// The session ends long before the command, which the client
				// must not take for a success.
				start := time.Now()
				streamed := make(chan error, 1)

				go func() {
					streamed <- streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
						Stdout: io.Discard,
						Stderr: io.Discard,
					})
				}()

				var pids []int

				Eventually(func() []int {
					pids = pidsWithCmdline(command)

					return pids
				}, 5*time.Second).Should(HaveLen(1))

				Eventually(streamed, 8*time.Second).Should(Receive(&err))
				Expect(time.Since(start)).To(BeNumerically("<", 6*time.Second))
				Expect(err).To(MatchError(ContainSubstring("idle timeout")))

				// The exec process gets killed, nothing would end it otherwise.
				Eventually(func() []int { return pidsWithCmdline(command) }, 5*time.Second).
					ShouldNot(ContainElement(pids[0]))
			}, SpecTimeout(specTimeout))
		})

		Describe("ServeAttachContainer over "+transport, func() {
			const terminal = true

			It(testName("should succeed", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				// The container only answers once attached, then it exits, which
				// ends the session.
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "read -r line; echo \"got $line\""},
					nil,
				)
				sut = tr.configGivenEnv()
				tr.createContainer(ctx, sut, terminal)
				tr.startContainer()

				result, err := sut.ServeAttachContainer(
					ctx,
					&client.ServeAttachContainerConfig{
						ID:     tr.ctrID,
						Stdin:  true,
						Stdout: true,
						Stderr: true,
					},
				)

				Expect(err).To(Succeed())
				Expect(result.URL).NotTo(BeEmpty())

				stdout := &syncBuffer{}
				stderr := &syncBuffer{}

				// The end of stdin ends a terminal session, so it stays open
				// until the answer arrived.
				stdinReader, stdinWriter := io.Pipe()

				go func() {
					defer GinkgoRecover()
					defer stdinWriter.Close()

					_, err := stdinWriter.Write([]byte("attached\n"))
					Expect(err).To(Succeed())
					Eventually(
						stdout.String,
						10*time.Second,
					).Should(ContainSubstring("got attached"))
				}()

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				Expect(streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
					Stdin:  stdinReader,
					Stdout: stdout,
					Stderr: stderr,
					Tty:    terminal,
				})).To(Succeed())
				Expect(stdout.String()).To(ContainSubstring("got attached"))
				Expect(stderr.String()).To(BeEmpty())
			}, SpecTimeout(specTimeout))

			It("should apply resize events and release the session", func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sh", "-c", sttyLoop})

				result, err := sut.ServeAttachContainer(
					ctx,
					&client.ServeAttachContainerConfig{ID: tr.ctrID, Stdout: true},
				)
				Expect(err).To(Succeed())

				// The streaming server and the session got set up.
				resources := resourcesOf(sut.PID())

				sizes := terminalSizes(remotecommand.TerminalSize{Width: 30, Height: 40})
				stdout := &syncBuffer{}
				streamCtx, cancel := context.WithCancel(ctx)

				defer cancel()

				streamed := make(chan error, 1)

				go func() {
					defer GinkgoRecover()

					streamed <- streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
						Stdout:            stdout,
						Tty:               terminal,
						TerminalSizeQueue: sizes,
					})
				}()

				// stty prints the rows, then the columns.
				Eventually(stdout.String, 10*time.Second).Should(ContainSubstring("40 30"))

				// The client goes away, the container keeps running.
				cancel()
				Eventually(streamed, 10*time.Second).Should(Receive())

				expectNoResourceLeak(sut.PID(), resources)
			}, SpecTimeout(specTimeout))

			It(
				"should receive the output written after stdin EOF with stdin once",
				func(ctx SpecContext) {
					runContainer(
						ctx,
						false,
						[]string{"/busybox", "sh", "-c", "/busybox cat && echo stdin closed"},
					)

					result, err := sut.ServeAttachContainer(
						ctx,
						&client.ServeAttachContainerConfig{
							ID:                tr.ctrID,
							Stdin:             true,
							Stdout:            true,
							Stderr:            true,
							StopAfterStdinEOF: true,
						},
					)
					Expect(err).To(Succeed())

					stdout := &bytes.Buffer{}
					stderr := &bytes.Buffer{}

					streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
					defer cancel()

					Expect(
						streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
							Stdin:  strings.NewReader("attached"),
							Stdout: stdout,
							Stderr: stderr,
						}),
					).To(Succeed())
					Expect(stdout.String()).To(Equal("attachedstdin closed\n"))
					Expect(stderr.String()).To(BeEmpty())
				},
				SpecTimeout(specTimeout),
			)

			It("should attach to a terminal without asking for one", func(ctx SpecContext) {
				runContainer(
					ctx,
					true,
					[]string{"/busybox", "sh", "-c", "/busybox sleep 1; echo hello"},
				)

				result, err := sut.ServeAttachContainer(
					ctx,
					&client.ServeAttachContainerConfig{
						ID:     tr.ctrID,
						Stdout: true,
					},
				)
				Expect(err).To(Succeed())

				stdout := &syncBuffer{}

				streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				defer cancel()

				// Without a terminal, the client opens no resize stream.
				Expect(streamURLOver(streamCtx, transport, result.URL, remotecommand.StreamOptions{
					Stdout: stdout,
				})).To(Succeed())
				Expect(stdout.String()).To(ContainSubstring("hello"))
			}, SpecTimeout(specTimeout))
		})
	}

	// The server looks the container up before it hands out a URL, whatever
	// the transport of the client.
	Describe("for a container which does not exist", func() {
		It("should fail to serve an exec", func(ctx SpecContext) {
			newServer()

			result, err := sut.ServeExecContainer(
				ctx,
				&client.ServeExecContainerConfig{
					ID:      "wrong",
					Command: []string{"/busybox", "true"},
					Stdout:  true,
					Stderr:  true,
				},
			)

			Expect(err).To(MatchError(client.ErrContainerNotFound))
			Expect(result).To(BeNil())
		}, SpecTimeout(specTimeout))

		It("should fail to serve an attach", func(ctx SpecContext) {
			newServer()

			result, err := sut.ServeAttachContainer(
				ctx,
				&client.ServeAttachContainerConfig{ID: "wrong", Stdout: true},
			)

			Expect(err).To(MatchError(client.ErrContainerNotFound))
			Expect(result).To(BeNil())
		}, SpecTimeout(specTimeout))
	})

	Describe("through the proxy of the kubelet", func() {
		It("should exec over SPDY", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

			result, err := sut.ServeExecContainer(
				ctx,
				&client.ServeExecContainerConfig{
					ID: tr.ctrID,
					Command: []string{
						"/busybox",
						"sh",
						"-c",
						"/busybox cat && echo -n stderr >&2 && exit 3",
					},
					Stdin:  true,
					Stdout: true,
					Stderr: true,
				},
			)
			Expect(err).To(Succeed())

			proxyURL, stop := kubeletProxy(result.URL)
			defer stop()

			stdout := &bytes.Buffer{}
			stderr := &bytes.Buffer{}

			streamCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
			defer cancel()

			err = streamURLOver(streamCtx, "SPDY", proxyURL, remotecommand.StreamOptions{
				Stdin:  strings.NewReader("stdin"),
				Stdout: stdout,
				Stderr: stderr,
			})

			var exitErr utilexec.CodeExitError
			Expect(errors.As(err, &exitErr)).To(BeTrue(), "error %v", err)
			Expect(exitErr.Code).To(Equal(3))
			Expect(stdout.String()).To(Equal("stdin"))
			Expect(stderr.String()).To(Equal("stderr"))
		}, SpecTimeout(specTimeout))

		It("should forward a port over SPDY", func(ctx SpecContext) {
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

			proxyURL, stopProxy := kubeletProxy(result.URL)
			defer stopProxy()

			dialer, err := portForwardDialer(proxyURL)
			Expect(err).NotTo(HaveOccurred())

			local, stop, err := startPortForward(dialer, port)
			Expect(err).NotTo(HaveOccurred())

			defer stop()

			Expect(portForwardRoundTrip(local, "hello")).To(Equal("hello"))
		}, SpecTimeout(specTimeout))
	})

	Describe("ServeExecContainer over older WebSocket protocols", func() {
		for _, protocol := range []string{"v4.channel.k8s.io", "v4.base64.channel.k8s.io"} {
			It("should report the output and the exit code over "+protocol, func(ctx SpecContext) {
				runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

				result, err := sut.ServeExecContainer(
					ctx,
					&client.ServeExecContainerConfig{
						ID: tr.ctrID,
						Command: []string{
							"/busybox",
							"sh",
							"-c",
							"echo -n stdout && echo -n stderr >&2 && exit 3",
						},
						Stdout: true,
						Stderr: true,
					},
				)
				Expect(err).To(Succeed())

				stdout, stderr, status, err := legacyExec(result.URL, protocol)
				Expect(err).NotTo(HaveOccurred())
				Expect(stdout).To(Equal("stdout"))
				Expect(stderr).To(Equal("stderr"))
				Expect(status).To(ContainSubstring(`"reason":"NonZeroExitCode"`))
				Expect(status).To(ContainSubstring(`{"reason":"ExitCode","message":"3"}`))
			}, SpecTimeout(specTimeout))
		}

		It("should report the exit code of a process killed by a signal", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

			result, err := sut.ServeExecContainer(
				ctx,
				&client.ServeExecContainerConfig{
					ID:      tr.ctrID,
					Command: []string{"/busybox", "sh", "-c", "kill -KILL $$"},
					Stdout:  true,
				},
			)
			Expect(err).To(Succeed())

			_, _, status, err := legacyExec(result.URL, "v4.channel.k8s.io")
			Expect(err).NotTo(HaveOccurred())
			// Like a shell reports it, 128 and the number of the signal.
			Expect(status).To(ContainSubstring(`"reason":"NonZeroExitCode"`))
			Expect(status).To(ContainSubstring(`{"reason":"ExitCode","message":"137"}`))
		}, SpecTimeout(specTimeout))
	})
})
