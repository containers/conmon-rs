package client_test

import (
	"context"
	"errors"
	"io"
	"net"
	"os"
	"path/filepath"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-spec/specs-go"
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
	runContainer := containerRunner(&tr, &sut)

	Describe("CreateContainer", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should create a simple container", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(ctx, sut, terminal)
			}, SpecTimeout(specTimeout))

			It(testName("should write exit file", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(ctx, sut, terminal)
				tr.startContainer()
				// The container may still run, the exit file comes with its
				// exit.
				Eventually(fileContents, 10*time.Second).
					WithArguments(tr.exitPath()).
					Should(Equal("0"))
			}, SpecTimeout(specTimeout))

			It(
				testName("should write a non-zero exit code to the exit file", terminal),
				func(ctx SpecContext) {
					runContainer(ctx, terminal, []string{"/busybox", "sh", "-c", "exit 3"})
					Eventually(fileContents, 10*time.Second).
						WithArguments(tr.exitPath()).
						Should(Equal("3"))
				},
				SpecTimeout(specTimeout),
			)

			It(
				testName("should execute cleanup command when container exits", terminal),
				func(ctx SpecContext) {
					tr = newTestRunner()
					tr.createRuntimeConfig(terminal)
					// The container ID is only known after the runtime config exists.
					cleanupPath := filepath.Join(tr.tmpDir, "cleanup-"+tr.ctrID)
					sut = tr.configGivenEnv()
					cfg := tr.defaultConfig(terminal)
					cfg.CleanupCmd = []string{"touch", cleanupPath}
					tr.createContainerWithConfig(ctx, sut, cfg)
					tr.startContainer()
					Eventually(cleanupPath, 10*time.Second).Should(BeAnExistingFile())
					Expect(fileContents(cleanupPath)).To(BeEmpty())
				},
				SpecTimeout(specTimeout),
			)

			It(testName("should return error if invalid command", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"invalid"}, nil)
				sut = tr.configGivenEnv()
				_, err := sut.CreateContainer(ctx, tr.defaultConfig(terminal))
				Expect(err).NotTo(Succeed())
				Expect(err.Error()).To(ContainSubstring(`not found in $PATH`))
			}, SpecTimeout(specTimeout))

			It(testName("should handle long run dir", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.tmpDir = MustDirInTempDir(
					tr.tmpDir,
					"thisisareallylongdirithasmanycharactersinthepathsosuperduperlongannoyinglylong",
				)
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(ctx, sut, terminal)
			}, SpecTimeout(specTimeout))

			It(
				testName("should succeed/error to set the window size", terminal),
				func(ctx SpecContext) {
					tr = newTestRunner()
					// Report the size of the terminal until the resize shows up.
					tr.createRuntimeConfigWithProcessArgs(
						terminal,
						[]string{
							"/busybox",
							"sh",
							"-c",
							sttyLoop,
						},
						nil,
					)
					sut = tr.configGivenEnv()
					tr.createContainer(ctx, sut, terminal)
					tr.startContainer()

					err := sut.SetWindowSizeContainer(
						ctx,
						&client.SetWindowSizeContainerConfig{
							ID: tr.ctrID,
							Size: &resize.TerminalSize{
								Width:  10,
								Height: 20,
							},
						},
					)
					if !terminal {
						Expect(err).To(MatchError(ContainSubstring("container has no terminal")))
						Expect(err).NotTo(MatchError(client.ErrContainerNotFound))

						return
					}

					Expect(err).To(Succeed())

					// stty prints the rows, then the columns.
					Eventually(fileContents, 10*time.Second).
						WithArguments(tr.logPath()).
						Should(ContainSubstring(" 20 10"))
				},
				SpecTimeout(specTimeout),
			)

			It(
				testName("should catch out of memory (oom) events", terminal),
				func(ctx SpecContext) {
					if unshare.IsRootless() {
						Skip("does not run rootless")
					}

					tr = newTestRunner()
					tr.createRuntimeConfigWithProcessArgs(
						terminal,
						[]string{"/busybox", "tail", "/dev/zero"},
						func(g generate.Generator) {
							// Pin the swap as well, otherwise the cgroup swaps on
							// hosts with swap instead of being killed.
							const limit = 1024 * 1024
							g.SetLinuxResourcesMemoryLimit(limit)
							g.SetLinuxResourcesMemorySwap(limit)
						},
					)
					sut = tr.configGivenEnv()
					tr.createContainer(ctx, sut, terminal)
					tr.startContainer()

					Eventually(tr.oomExitPath, 10*time.Second).Should(BeAnExistingFile())
					Expect(fileContents(tr.oomExitPath())).To(BeEmpty())

					// Killed by SIGKILL, like a shell reports it.
					Eventually(fileContents, 10*time.Second).
						WithArguments(tr.exitPath()).
						Should(Equal("137"))
				},
				SpecTimeout(specTimeout),
			)

			It(testName("should reopen logs based on max size", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				// The last line has no newline, which a terminal may pass on
				// in a read of its own, its entry would rotate the log again.
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo -n world"},
					nil,
				)
				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers[0].MaxSize = 50
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// The second line exceeds the size and rotates the log, so
				// only it survives, as a partial line.
				Eventually(fileContents, 5*time.Second).
					WithArguments(tr.logPath()).
					Should(ContainSubstring(" P world"))
				Expect(fileContents(tr.logPath())).NotTo(ContainSubstring("hello"))
			}, SpecTimeout(specTimeout))
			It(testName("should respect global args", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)
				logFile := MustFile(filepath.Join(tr.tmpDir, "runtime-log"))
				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.GlobalArgs = []string{"--log", logFile, "--debug"}
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				Expect(fileContents(logFile)).NotTo(BeEmpty())
			}, SpecTimeout(specTimeout))
		}

		It("should keep the output of a terminal which got opened again", func(ctx SpecContext) {
			// Like an init system which closes the console and opens it
			// again, the output must not end meanwhile.
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(true, []string{
				"/busybox", "sh", "-c",
				"echo first; exec </dev/null >/dev/null 2>&1; " +
					"/busybox sleep 1; echo second >/dev/console",
			}, nil)
			sut = tr.configGivenEnv()
			tr.createContainer(ctx, sut, true)
			tr.startContainer()

			Eventually(fileContents, 10*time.Second).
				WithArguments(tr.exitPath()).
				Should(Equal("0"))
			Expect(fileContents(tr.logPath())).To(And(
				ContainSubstring("first"),
				ContainSubstring("second"),
			))
		}, SpecTimeout(specTimeout))

		// sendPipe sends the write end of a new pipe to the fd socket of the
		// server and closes it, so that only the server holds it then. It
		// returns the read end and the slot of the write end. The fd socket
		// gets closed with the spec, which releases the slots the container
		// did not take.
		sendPipe := func(ctx context.Context) (*os.File, client.RemoteFD) {
			remoteFDs, err := sut.RemoteFDs(ctx)
			Expect(err).To(Succeed())
			DeferCleanup(remoteFDs.Close)

			reader, writer, err := os.Pipe()
			Expect(err).To(Succeed())
			DeferCleanup(reader.Close)

			slots, err := remoteFDs.SendContext(ctx, int(writer.Fd()))
			Expect(err).To(Succeed())
			Expect(slots).To(HaveLen(1))
			Expect(writer.Close()).To(Succeed())

			return reader, slots[0]
		}

		It("should pass additional file descriptors to the container", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "sh", "-c", "echo passed >&3"},
				nil,
			)
			sut = tr.configGivenEnv()

			reader, slot := sendPipe(ctx)

			// The runtime gets the descriptor as fd 3 and passes it on.
			cfg := tr.defaultConfig(false)
			cfg.AdditionalFDs = []client.RemoteFD{slot}
			cfg.CommandArgs = []string{"--preserve-fds", "1"}
			tr.createContainerWithConfig(ctx, sut, cfg)
			tr.startContainer()

			// Neither the server nor the runtime keep a copy, the output
			// ends with the container.
			Expect(reader.SetReadDeadline(time.Now().Add(10 * time.Second))).To(Succeed())
			Expect(io.ReadAll(reader)).To(BeEquivalentTo("passed\n"))
		}, SpecTimeout(specTimeout))

		It("should hold leaked file descriptors until the container exits", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			reader, slot := sendPipe(ctx)

			cfg := tr.defaultConfig(false)
			cfg.LeakFDs = []client.RemoteFD{slot}
			tr.createContainerWithConfig(ctx, sut, cfg)

			// The server holds it while the container waits for its start.
			Expect(reader.SetReadDeadline(time.Now().Add(500 * time.Millisecond))).To(Succeed())

			_, err := reader.Read(make([]byte, 1))
			Expect(err).To(MatchError(os.ErrDeadlineExceeded))

			// And closes it once the container exited.
			tr.startContainer()
			Expect(reader.SetReadDeadline(time.Now().Add(10 * time.Second))).To(Succeed())

			_, err = reader.Read(make([]byte, 1))
			Expect(err).To(MatchError(io.EOF))
			Eventually(fileContents, 5*time.Second).WithArguments(tr.exitPath()).Should(Equal("0"))
		}, SpecTimeout(specTimeout))

		It("should relay sd_notify messages", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "sh", "-c", "/busybox ls $NOTIFY_SOCKET"},
				func(g generate.Generator) {
					// What the container engine sets up for the container.
					g.AddMount(specs.Mount{
						Destination: "/run/notify",
						Source:      filepath.Join(tr.tmpDir, "notify"),
						Options:     []string{"bind", "ro"},
					})
					g.AddProcessEnv("NOTIFY_SOCKET", "/run/notify/notify.sock")
				},
			)
			sut = tr.configGivenEnv()

			// The host side, usually the notify socket of systemd.
			hostPath := filepath.Join(tr.tmpDir, "host.sock")
			host, err := net.ListenUnixgram(
				"unixgram",
				&net.UnixAddr{Name: hostPath, Net: "unixgram"},
			)
			Expect(err).To(Succeed())

			defer host.Close()

			cfg := tr.defaultConfig(false)
			cfg.SdNotifySocket = hostPath
			tr.createContainerWithConfig(ctx, sut, cfg)

			// busybox cannot send datagrams, so send from the host side what
			// the container would send through the mount.
			ctrPath := filepath.Join(tr.tmpDir, "notify", "notify.sock")
			conn, err := net.DialUnix(
				"unixgram",
				nil,
				&net.UnixAddr{Name: ctrPath, Net: "unixgram"},
			)
			Expect(err).To(Succeed())

			defer conn.Close()

			const message = "READY=1\nSTATUS=up"

			_, err = conn.Write([]byte(message))
			Expect(err).To(Succeed())

			Expect(host.SetReadDeadline(time.Now().Add(10 * time.Second))).To(Succeed())

			buf := make([]byte, 4096)
			n, err := host.Read(buf)
			Expect(err).To(Succeed())
			Expect(string(buf[:n])).To(Equal(message))

			// The socket existed before the runtime mounted its directory.
			tr.startContainer()
			Eventually(fileContents, 5*time.Second).
				WithArguments(tr.logPath()).
				Should(ContainSubstring("/run/notify/notify.sock"))

			// The socket goes away with the container.
			Eventually(func() bool {
				_, err := os.Stat(ctrPath)

				return errors.Is(err, os.ErrNotExist)
			}, time.Second*5).Should(BeTrue())
		}, SpecTimeout(specTimeout))
	})

	Describe("ReopenLogContainer", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should continue the log in a new file", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{
					"/busybox",
					"sh",
					"-c",
					"i=0; while true; do echo line $i; i=$((i+1)); /busybox sleep 0.1; done",
				})

				atLeastThreeLines := WithTransform(
					func(lines []int) int { return len(lines) },
					BeNumerically(">=", 3),
				)
				Eventually(
					func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
					10*time.Second,
				).
					Should(atLeastThreeLines)

				// Rotate the log like a log rotation would do.
				rotated := tr.logPath() + ".1"
				Expect(os.Rename(tr.logPath(), rotated)).To(Succeed())
				Expect(
					sut.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
						ID: tr.ctrID,
					}),
				).To(Succeed())

				// The reopen wrote everything before it to the old file.
				oldLines := logLineNumbers(Default, rotated)
				Expect(oldLines).To(atLeastThreeLines)

				Eventually(
					func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
					10*time.Second,
				).
					Should(atLeastThreeLines)

				newLines := logLineNumbers(Default, tr.logPath())

				// The old file gets no more lines, the new one continues
				// right after it.
				Expect(logLineNumbers(Default, rotated)).To(Equal(oldLines))
				Expect(oldLines[0]).To(Equal(0))

				for _, lines := range [][]int{oldLines, newLines} {
					for i := 1; i < len(lines); i++ {
						Expect(lines[i]).To(Equal(lines[i-1]+1), "lines %v", lines)
					}
				}

				Expect(newLines[0]).To(Equal(oldLines[len(oldLines)-1] + 1))
			}, SpecTimeout(specTimeout))
		}

		It("should fail for an unknown container", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			Expect(sut.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
				ID: "unknown",
			})).To(MatchError(client.ErrContainerNotFound))

			// The failed request keeps the server usable.
			_, err := sut.Version(ctx, &client.VersionConfig{})
			Expect(err).To(Succeed())
		}, SpecTimeout(specTimeout))
	})
})
