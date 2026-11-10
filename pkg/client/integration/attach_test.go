package client_test

import (
	"context"
	"io"
	"path/filepath"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-tools/generate"
	"go.podman.io/common/pkg/resize"
	"go.podman.io/storage/pkg/unshare"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)
	newServer := serverStarter(&tr, &sut)
	runContainer := containerRunner(&tr, &sut)

	Describe("Attach", func() {
		It("should fail to attach to an unknown container", func(ctx SpecContext) {
			newServer()

			socketPath := filepath.Join(tr.tmpDir, "attach")
			Expect(sut.AttachContainer(ctx, &client.AttachContainerConfig{
				ID:         "unknown",
				SocketPath: socketPath,
			})).To(MatchError(client.ErrContainerNotFound))
			Expect(socketPath).NotTo(BeAnExistingFile())
		}, SpecTimeout(specTimeout))

		matrix := []struct {
			terminal       bool
			pipe           string
			leaveStdinOpen bool
		}{
			{
				terminal: true,
				pipe:     "stdout",
			},
			{
				terminal: false,
				pipe:     "stdout",
			},
			{
				terminal: false,
				pipe:     "stderr",
			},
			// CRI-O keeps stdin open unless stdinOnce is set, output has to
			// keep flowing until the container exits.
			{
				terminal:       true,
				pipe:           "stdout",
				leaveStdinOpen: true,
			},
			{
				terminal:       false,
				pipe:           "stdout",
				leaveStdinOpen: true,
			},
		}
		for _, test := range matrix {
			terminal := test.terminal
			pipe := test.pipe
			leaveStdinOpen := test.leaveStdinOpen

			name := "should succeed with " + test.pipe
			if leaveStdinOpen {
				name += " leaving stdin open"
			}

			It(testName(name, test.terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sh"})

				if terminal {
					// Input before the shell put the terminal into raw mode
					// gets echoed by the terminal as well.
					Eventually(fileContents, 5*time.Second).
						WithArguments(tr.logPath()).
						Should(ContainSubstring("# "))
				}

				stdinReader, stdinWriter := io.Pipe()
				stdReader, stdWriter := io.Pipe()

				cfg := &client.AttachContainerConfig{
					ID:                tr.ctrID,
					SocketPath:        filepath.Join(tr.tmpDir, "attach"),
					ContainerStdin:    true,
					StopAfterStdinEOF: !leaveStdinOpen,
					Streams: client.AttachStreams{
						Stdin: stdinReader,
					},
				}
				if pipe == "stdout" {
					cfg.Streams.Stdout = stdWriter
				} else {
					cfg.Streams.Stderr = stdWriter
				}

				testAttach(ctx, sut, cfg, stdinWriter, stdReader, pipe, pipe == "stderr", terminal)
			}, SpecTimeout(specTimeout))
		}

		// attachAll attaches stdin, stdout and stderr to a container running the
		// script, writes the input and closes stdin if requested. The
		// beforeAttach function runs after starting the container.
		attachAll := func(
			ctx context.Context,
			script string, terminal, stopAfterStdinEOF bool, input string, closeStdin bool,
			changeSpec func(generate.Generator), beforeAttach func(),
		) (attachErr <-chan error, stdout, stderr <-chan string) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(
				terminal,
				[]string{"/busybox", "sh", "-c", script},
				changeSpec,
			)
			sut = tr.configGivenEnv()
			tr.createContainer(ctx, sut, terminal)
			tr.startContainer()

			if beforeAttach != nil {
				beforeAttach()
			}

			stdinReader, stdinWriter := io.Pipe()
			stdoutReader, stdoutWriter := io.Pipe()
			stderrReader, stderrWriter := io.Pipe()

			cfg := &client.AttachContainerConfig{
				ID:                tr.ctrID,
				SocketPath:        filepath.Join(tr.tmpDir, "attach"),
				Terminal:          terminal,
				StopAfterStdinEOF: stopAfterStdinEOF,
				ContainerStdin:    true,
				Streams: client.AttachStreams{
					Stdin:  stdinReader,
					Stdout: stdoutWriter,
					Stderr: stderrWriter,
				},
			}

			readAll := func(reader io.Reader) <-chan string {
				output := make(chan string, 1)

				go func() {
					defer GinkgoRecover()

					data, err := io.ReadAll(reader)
					Expect(err).To(Succeed())

					output <- string(data)
				}()

				return output
			}

			go func() {
				defer GinkgoRecover()

				if input != "" {
					_, err := stdinWriter.Write([]byte(input))
					Expect(err).To(Succeed())
				}

				if closeStdin {
					Expect(stdinWriter.Close()).To(Succeed())
				}
			}()

			errCh := make(chan error, 1)

			go func() {
				errCh <- sut.AttachContainer(ctx, cfg)
			}()

			return errCh, readAll(stdoutReader), readAll(stderrReader)
		}

		It("should receive the output written after stdin EOF", func(ctx SpecContext) {
			attachErr, stdout, stderr := attachAll(
				ctx,
				"/busybox cat && echo stdin closed", false, true, "attached", true, nil, nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(Equal("attachedstdin closed\n")))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		}, SpecTimeout(specTimeout))

		It("should receive all output if stdin stays open", func(ctx SpecContext) {
			attachErr, stdout, stderr := attachAll(
				ctx,
				"read -r; echo one; /busybox sleep 0.5; echo two",
				false,
				false,
				"go\n",
				false,
				nil,
				nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(Equal("one\ntwo\n")))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		}, SpecTimeout(specTimeout))

		It("should return on stdin EOF if stdin stays open", func(ctx SpecContext) {
			attachErr, stdout, stderr := attachAll(
				ctx,
				"/busybox sleep 1000",
				false,
				false,
				"",
				true,
				nil,
				nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(BeEmpty()))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		}, SpecTimeout(specTimeout))

		It("should return on stdin EOF with terminal and stdin once", func(ctx SpecContext) {
			attachErr, _, _ := attachAll(ctx, "/busybox sleep 1000", true, true, "", true, nil, nil)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
		}, SpecTimeout(specTimeout))

		It(
			"should attach again to the same socket after an attach returned",
			func(ctx SpecContext) {
				runContainer(
					ctx,
					false,
					[]string{"/busybox", "sh", "-c", "while read -r line; do echo got $line; done"},
				)

				socketPath := filepath.Join(tr.tmpDir, "attach")

				// What the server holds once it listens on the socket, the
				// later attaches must not add to it.
				var resources serverResources

				for i, input := range []string{"one", "two", "three"} {
					stdinReader, stdinWriter := io.Pipe()
					stdoutReader, stdoutWriter := io.Pipe()

					output := make(chan string, 10)

					go func() {
						defer GinkgoRecover()

						data := make([]byte, 64)

						for {
							n, err := stdoutReader.Read(data)
							if n > 0 {
								output <- string(data[:n])
							}

							if err != nil {
								return
							}
						}
					}()

					attachErr := make(chan error, 1)

					go func() {
						attachErr <- sut.AttachContainer(ctx, &client.AttachContainerConfig{
							ID:             tr.ctrID,
							SocketPath:     socketPath,
							ContainerStdin: true,
							Streams: client.AttachStreams{
								Stdin:  stdinReader,
								Stdout: stdoutWriter,
							},
						})
					}()

					_, err := stdinWriter.Write([]byte(input + "\n"))
					Expect(err).To(Succeed())
					Eventually(output, 10*time.Second).Should(Receive(Equal("got " + input + "\n")))

					// Without stdin once, the stdin EOF ends the attach and the
					// socket stays for the next one.
					Expect(stdinWriter.Close()).To(Succeed())
					Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
					Expect(socketPath).To(BeAnExistingFile())

					if i == 0 {
						resources = resourcesOf(sut.PID())
					}
				}

				expectNoResourceLeak(sut.PID(), resources)
			},
			SpecTimeout(specTimeout),
		)

		It("should apply resize events", func(ctx SpecContext) {
			runContainer(ctx, true, []string{"/busybox", "sh", "-c", sttyLoop})

			stdoutReader, stdoutWriter := io.Pipe()
			stdout := &syncBuffer{}

			go io.Copy(stdout, stdoutReader) //nolint:errcheck // ends with the attach

			resizeEvents := make(chan resize.TerminalSize, 1)
			resizeEvents <- resize.TerminalSize{Width: 30, Height: 40}

			attachCtx, cancel := context.WithCancel(ctx)
			defer cancel()

			attachErr := make(chan error, 1)

			go func() {
				attachErr <- sut.AttachContainer(attachCtx, &client.AttachContainerConfig{
					ID:         tr.ctrID,
					SocketPath: filepath.Join(tr.tmpDir, "attach"),
					Terminal:   true,
					Resize:     resizeEvents,
					Streams:    client.AttachStreams{Stdout: stdoutWriter},
				})
			}()

			// stty prints the rows, then the columns.
			Eventually(stdout.String, 10*time.Second).Should(ContainSubstring("40 30"))

			cancel()
			Eventually(attachErr, 10*time.Second).Should(Receive(MatchError(context.Canceled)))
		}, SpecTimeout(specTimeout))

		It("should return once the context is done", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "1000"})

			stdinReader, stdinWriter := io.Pipe()
			defer stdinWriter.Close()

			stdoutReader, stdoutWriter := io.Pipe()

			// The output stream gets closed once the attach is over.
			outputClosed := make(chan error, 1)

			go func() {
				_, err := io.Copy(io.Discard, stdoutReader)
				outputClosed <- err
			}()

			attachCtx, cancel := context.WithTimeout(ctx, time.Second)
			defer cancel()

			attachErr := make(chan error, 1)

			go func() {
				attachErr <- sut.AttachContainer(attachCtx, &client.AttachContainerConfig{
					ID:                tr.ctrID,
					SocketPath:        filepath.Join(tr.tmpDir, "attach"),
					ContainerStdin:    true,
					StopAfterStdinEOF: true,
					Streams: client.AttachStreams{
						Stdin:  stdinReader,
						Stdout: stdoutWriter,
					},
				})
			}()

			Eventually(
				attachErr,
				10*time.Second,
			).Should(Receive(MatchError(context.DeadlineExceeded)))
			// The attach closed the output before it returned.
			Eventually(outputClosed, time.Second).Should(Receive(Succeed()))
		}, SpecTimeout(specTimeout))

		It(
			"should receive the done packet if the output ended before attaching",
			func(ctx SpecContext) {
				attachErr, stdout, stderr := attachAll(
					ctx,
					"exec >&- 2>&-; /busybox touch /output-closed; /busybox cat",
					false,
					true,
					"",
					true,
					nil,
					func() {
						// Attach only once the output got closed, the read loops
						// see the EOF right away.
						Eventually(func() string {
							return filepath.Join(tr.tmpRootfs, "output-closed")
						}, 10*time.Second).Should(BeAnExistingFile())
					},
				)

				Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
				Eventually(stdout, 10*time.Second).Should(Receive(BeEmpty()))
				Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
			},
			SpecTimeout(specTimeout),
		)

		It("should catch out of memory (oom) events after stdin EOF", func(ctx SpecContext) {
			if unshare.IsRootless() {
				Skip("does not run rootless")
			}

			attachErr, _, _ := attachAll(
				ctx, "/busybox cat; /busybox tail /dev/zero", false, true, "", true, oomSpec, nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(tr.oomExitPath, 10*time.Second).Should(BeAnExistingFile())
			Eventually(fileContents, 10*time.Second).
				WithArguments(tr.exitPath()).
				Should(Equal("137"))
		}, SpecTimeout(specTimeout))
	})
})
