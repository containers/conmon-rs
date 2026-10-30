package client_test

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/google/uuid"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-spec/specs-go"
	"github.com/opencontainers/runtime-tools/generate"
	"go.podman.io/common/pkg/resize"
	"go.podman.io/storage/pkg/idtools"
	"go.podman.io/storage/pkg/unshare"
	"k8s.io/client-go/tools/portforward"
	"k8s.io/client-go/tools/remotecommand"
	utilexec "k8s.io/client-go/util/exec"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	// A spec which fails or skips before it created its server must not
	// make the hooks below look at the server of the previous spec.
	BeforeEach(func() {
		tr = nil
		sut = nil
	})

	JustAfterEach(func() {
		if sut != nil {
			pid := sut.PID()
			Expect(pid).To(BeNumerically(">", 0))
			rss := vmRSSGivenPID(pid)
			// Named after the spec, so that a value can be attributed in the
			// verbose output of a whole run.
			GinkgoWriter.Printf(
				"VmRSS for server is %d kB in %q\n",
				rss,
				CurrentSpecReport().FullText(),
			)
			Expect(rss).To(BeNumerically("<", maxRSSKB), "VmRSS of the server in kB")
		}
	})

	AfterEach(func() {
		cleanupSpec(tr, sut)
	})

	// runContainer starts a server with a started container which runs the
	// process.
	runContainer := func(ctx context.Context, terminal bool, processArgs []string) {
		tr = newTestRunner()
		tr.createRuntimeConfigWithProcessArgs(terminal, processArgs, nil)
		sut = tr.configGivenEnv()
		tr.createContainer(ctx, sut, terminal)
		tr.startContainer()
	}

	Describe("New", func() {
		It("should restore from running server", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()
			sut2 := tr.configGivenEnv()
			Expect(sut2.PID()).To(Equal(sut.PID()))

			// Only the client goes away, the cleanup stops the server.
			Expect(sut2.Close()).To(Succeed())

			_, err := sut.Version(ctx, &client.VersionConfig{})
			Expect(err).To(Succeed())
		}, SpecTimeout(specTimeout))

		It("should start with an output writer which is no file", func(_ SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)

			// The server outlives New and keeps writing to it.
			output := &syncBuffer{}
			started := make(chan *client.ConmonClient, 1)
			finished := make(chan struct{})

			go func() {
				defer GinkgoRecover()
				defer close(finished)

				started <- tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
					cfg.Stdout = output
					cfg.Stderr = output
				})
			}()

			// A client which New returns once the wait below gave up still
			// has a server to stop. New itself gives up after 20 seconds.
			DeferCleanup(func() {
				Eventually(finished, 30*time.Second).Should(BeClosed())

				select {
				case late := <-started:
					cleanupSpec(nil, late)
				default:
				}
			})

			Eventually(started, 10*time.Second).Should(Receive(&sut))
			Eventually(output.String, 10*time.Second).ShouldNot(BeEmpty())
		}, SpecTimeout(specTimeout))

		It("should start the server with the given environment", func(_ SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.Env = []string{"CONMON_TEST_ENV=set"}
			})

			contents, err := fileContents(fmt.Sprintf("/proc/%d/environ", sut.PID()))
			Expect(err).To(Succeed())

			environ := strings.Split(contents, "\x00")
			Expect(environ).To(ContainElement("CONMON_TEST_ENV=set"))
			Expect(environ).NotTo(ContainElement(HavePrefix("PATH=")))
		}, SpecTimeout(specTimeout))

		It("should not keep the launch stdio without the stdout log driver", func(_ SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.LogDriver = client.LogDriverNone
			})

			// Like the processes it starts, which inherit them.
			for fd := range 3 {
				Expect(os.Readlink(fmt.Sprintf("/proc/%d/fd/%d", sut.PID(), fd))).
					To(Equal(os.DevNull))
			}
		}, SpecTimeout(specTimeout))

		It("should start a new server once the old one got killed", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			old := sut
			oldPID := old.PID()
			socket := filepath.Join(tr.tmpDir, "conmon.sock")

			// A link keeps the inode of the old socket, which a new socket
			// would otherwise get again once the old one is gone.
			oldSocket := socket + ".old"
			Expect(os.Link(socket, oldSocket)).To(Succeed())

			// A crash leaves the socket behind.
			Expect(syscall.Kill(int(oldPID), syscall.SIGKILL)).To(Succeed())
			Eventually(func() error {
				return syscall.Kill(int(oldPID), 0)
			}, 10*time.Second).Should(MatchError(syscall.ESRCH))
			Expect(socket).To(BeAnExistingFile())

			// The client of the old server fails fast, it does not wait for
			// an answer.
			start := time.Now()
			_, err := old.Version(ctx, &client.VersionConfig{})
			Expect(err).To(HaveOccurred())
			Expect(time.Since(start)).To(BeNumerically("<", time.Second))

			// The cleanup shuts down the new server.
			sut = tr.configGivenEnv()
			Expect(sut.PID()).NotTo(Equal(oldPID))

			oldInfo, err := os.Lstat(oldSocket)
			Expect(err).NotTo(HaveOccurred())
			newInfo, err := os.Lstat(socket)
			Expect(err).NotTo(HaveOccurred())
			Expect(os.SameFile(oldInfo, newInfo)).To(BeFalse(), "stale socket not replaced")

			_, err = sut.Version(ctx, &client.VersionConfig{})
			Expect(err).To(Succeed())

			Expect(old.Close()).To(Succeed())
			_, err = old.Version(ctx, &client.VersionConfig{})
			Expect(err).To(MatchError(client.ErrClientClosed))
		}, SpecTimeout(specTimeout))
	})

	Describe("Shutdown", func() {
		// startServer starts a server with a created container and its fd
		// socket, and returns its PID.
		startServer := func(ctx context.Context, terminal bool) uint32 {
			tr = newTestRunner()
			tr.createRuntimeConfig(terminal)
			sut = tr.configGivenEnv()
			tr.createContainer(ctx, sut, terminal)

			remoteFDs, err := sut.RemoteFDs(ctx)
			Expect(err).To(Succeed())
			Expect(remoteFDs.Close()).To(Succeed())

			Expect(filepath.Join(tr.tmpDir, "conmon.sock")).To(BeAnExistingFile())
			Expect(filepath.Join(tr.tmpDir, "conmon-fd.sock")).To(BeAnExistingFile())

			return sut.PID()
		}

		// expectServerGone checks that the server is gone and removed its
		// files.
		expectServerGone := func(pid uint32) {
			Eventually(func() error {
				return syscall.Kill(int(pid), 0)
			}, 10*time.Second).Should(MatchError(syscall.ESRCH))
			// The server PID file is removed by New already, the "pidfile"
			// in the run directory belongs to the container of the bundle.
			Expect(filepath.Join(tr.tmpDir, "conmon.sock")).NotTo(BeAnExistingFile())
			Expect(filepath.Join(tr.tmpDir, "conmon-fd.sock")).NotTo(BeAnExistingFile())

			// The runtime reports the killed container as stopped, or
			// not at all once it got deleted.
			Eventually(func() bool {
				stopped := tr.rr.RunCommandCheckOutput("stopped", "list") == nil
				// Only a listing without the container counts, not a
				// failed listing.
				gone := errors.Is(tr.rr.RunCommandCheckOutput(tr.ctrID, "list"), errNoMatch)

				return stopped || gone
			}, time.Second*5).Should(BeTrue(), "container still created")
		}

		for _, terminal := range []bool{true, false} {
			It(
				testName("should kill created children if being killed", terminal),
				func(ctx SpecContext) {
					pid := startServer(ctx, terminal)

					// Shutdown should kill the container and clean up
					Expect(sut.Shutdown()).To(Succeed())
					sut = nil

					expectServerGone(pid)
				},
				SpecTimeout(specTimeout),
			)
		}

		It("should kill created children on SIGTERM", func(ctx SpecContext) {
			pid := startServer(ctx, false)
			Expect(syscall.Kill(int(pid), syscall.SIGTERM)).To(Succeed())

			// The hooks must not look at the server, which goes away by
			// itself, only the client is left to release.
			DeferCleanup(sut.Close)

			sut = nil

			expectServerGone(pid)
		}, SpecTimeout(specTimeout))

		It("should leave created children behind if being killed", func(ctx SpecContext) {
			pid := startServer(ctx, false)

			// Closing the client of the killed server is safe. A failed kill
			// leaves the server to the cleanup.
			DeferCleanup(sut.Close)
			Expect(sut.KillContext(ctx)).To(Succeed())

			sut = nil

			// The kill returns once the server is a zombie, which its parent
			// reaps later.
			Eventually(func() error {
				return syscall.Kill(int(pid), 0)
			}, 10*time.Second).Should(MatchError(syscall.ESRCH))

			// The server could not clean up: its socket and the container
			// stay, the container has no monitor anymore.
			Expect(filepath.Join(tr.tmpDir, "conmon.sock")).To(BeAnExistingFile())
			Expect(tr.rr.RunCommandCheckOutput(tr.ctrID, "list")).To(Succeed())

			// A new server replaces the socket left behind, it does not know
			// the container. The cleanup deletes the container and stops the
			// new server.
			sut = tr.configGivenEnv()
			Expect(sut.PID()).NotTo(Equal(pid))

			err := sut.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{ID: tr.ctrID})
			Expect(err).To(MatchError(client.ErrContainerNotFound))
		}, SpecTimeout(specTimeout))
	})

	Describe("Version", func() {
		for _, verbose := range []bool{false, true} {
			name := "should succeed"
			if verbose {
				name += " with verbose output"
			}

			It(name, func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfig(false)
				sut = tr.configGivenEnv()

				version, err := sut.Version(
					ctx,
					&client.VersionConfig{Verbose: verbose},
				)
				Expect(err).To(Succeed())

				Expect(version.ProcessID).NotTo(BeZero())
				Expect(version.Version).NotTo(BeEmpty())
				Expect(version.Commit).NotTo(BeEmpty())
				Expect(version.BuildDate).NotTo(BeEmpty())
				Expect(version.Target).NotTo(BeEmpty())
				Expect(version.RustVersion).NotTo(BeEmpty())
				Expect(version.CargoVersion).NotTo(BeEmpty())
				Expect(version.Features).To(ContainElements(
					client.FeatureSdNotify,
					client.FeaturePortForward,
					client.FeatureIdleTimeout,
					client.FeatureStreamingProtocols,
				))

				if verbose {
					Expect(version.CargoTree).NotTo(BeEmpty())
				} else {
					Expect(version.CargoTree).To(BeEmpty())
				}
			}, SpecTimeout(specTimeout))
		}
	})

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
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)
				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers[0].MaxSize = 50
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// The second line exceeds the size and rotates the log, so
				// only it survives.
				Eventually(fileContents, 5*time.Second).
					WithArguments(tr.logPath()).
					Should(ContainSubstring("world"))
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

	Describe("ExecSync Stress", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should handle many requests", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "30"})

				resources := resourcesOf(sut.PID())
				execCtx := withoutDeadline(ctx)

				var wg sync.WaitGroup
				for i := range 10 {
					wg.Add(1)

					go func(i int) {
						defer GinkgoRecover()
						defer wg.Done()

						result, err := sut.ExecSyncContainer(
							execCtx,
							&client.ExecSyncConfig{
								ID: tr.ctrID,
								Command: []string{
									"/busybox",
									"echo",
									"-n",
									"hello",
									"world",
									strconv.Itoa(i),
								},
								Terminal: terminal,
								Timeout:  timeoutUnlimited,
							},
						)
						Expect(err).To(Succeed())
						Expect(result).NotTo(BeNil())
						Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
						GinkgoWriter.Println("done with", i, string(result.Stdout))
					}(i)
				}

				wg.Wait()

				expectNoResourceLeak(sut.PID(), resources)
			}, SpecTimeout(specTimeout))

			It(testName("should not leak memory", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "600"})

				pid := sut.PID()
				execCtx := withoutDeadline(ctx)
				execEcho := func(i int) {
					result, err := sut.ExecSyncContainer(
						execCtx,
						&client.ExecSyncConfig{
							ID: tr.ctrID,
							Command: []string{
								"/busybox",
								"echo",
								"-n",
								"hello",
								"world",
								strconv.Itoa(i),
							},
							Terminal: terminal,
							Timeout:  timeoutUnlimited,
						},
					)

					Expect(err).To(Succeed())
					Expect(result).NotTo(BeNil())
					Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
					GinkgoWriter.Println("done with", i, string(result.Stdout))
				}

				// The first execs grow the heap of the allocator once, which
				// on AArch64 can take a whole transparent huge page of 2 MiB.
				// That is no leak, the measurement starts after them.
				const warmUps = 5
				for i := range warmUps {
					execEcho(i)
				}

				rssBefore := vmRSSGivenPID(pid)
				GinkgoWriter.Printf("VmRSS before: %d\n", rssBefore)
				logMemoryDetails(pid, "before")
				mappingsBefore := mappingRSS(pid)
				resources := resourcesOf(pid)

				for i := range 25 {
					execEcho(warmUps + i)
				}

				rssAfter := vmRSSGivenPID(pid)
				GinkgoWriter.Printf("VmRSS after: %d\n", rssAfter)
				logMemoryDetails(pid, "after")
				logGrownMappings(mappingsBefore, mappingRSS(pid))
				GinkgoWriter.Printf("VmRSS diff: %d\n", rssAfter-rssBefore)
				Expect(rssAfter - rssBefore).To(BeNumerically("<", maxRSSGrowthKB))

				expectNoResourceLeak(pid, resources)
			}, SpecTimeout(specTimeout))
		}
	})

	Describe("ExecSyncContainer", func() {
		It("should not start a command in an unknown container", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			// The runtime fails, which the result tells, not an error.
			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
				ID:      "unknown",
				Command: []string{"/busybox", "true"},
				Timeout: 10,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(Equal(client.ExecExitCodeNotStarted))
			Expect(result.TimedOut).To(BeFalse())
			Expect(result.Stderr).NotTo(BeEmpty())
		}, SpecTimeout(specTimeout))

		It("should pass the environment variables to the runtime", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(false, []string{"/busybox", "sleep", "10"}, nil)

			// A runtime which records the variable of its exec calls, the
			// container process does not get it.
			envFile := filepath.Join(tr.tmpDir, "runtime-env")
			wrapper := filepath.Join(tr.tmpDir, "runtime")
			script := fmt.Sprintf(
				"#!/bin/sh\n"+
					"if [ -n \"$CONMON_TEST_ENV\" ]; then printf %%s \"$CONMON_TEST_ENV\" >'%s'; fi\n"+
					"exec '%s' \"$@\"\n",
				envFile, runtimePath,
			)
			//nolint:gosec // the runtime has to be executable
			Expect(os.WriteFile(wrapper, []byte(script), 0o700)).To(Succeed())

			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.Runtime = wrapper
			})
			tr.createContainer(ctx, sut, false)
			tr.startContainer()
			Expect(envFile).NotTo(BeAnExistingFile())

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "true"},
				Timeout: 10,
				EnvVars: map[string]string{"CONMON_TEST_ENV": "set"},
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0))
			Expect(fileContents(envFile)).To(Equal("set"))
		}, SpecTimeout(specTimeout))

		It("should pass the cgroup manager of each call to the runtime", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(false, []string{"/busybox", "sleep", "10"}, nil)

			// A runtime which records the arguments of its calls, one per
			// line.
			argsFile := filepath.Join(tr.tmpDir, "runtime-args")
			wrapper := filepath.Join(tr.tmpDir, "runtime")
			script := fmt.Sprintf(
				"#!/bin/sh\n"+
					"printf '%%s\\n' \"$*\" >>'%s'\n"+
					"exec '%s' \"$@\"\n",
				argsFile, runtimePath,
			)
			//nolint:gosec // the runtime has to be executable
			Expect(os.WriteFile(wrapper, []byte(script), 0o700)).To(Succeed())

			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.Runtime = wrapper
				cfg.CgroupManager = client.CgroupManagerPerCommand
			})
			cfg := tr.defaultConfig(false)
			cfg.CgroupManager = client.CgroupManagerCgroupfs
			tr.createContainerWithConfig(ctx, sut, cfg)
			tr.startContainer()

			for _, manager := range []client.CgroupManager{
				client.CgroupManagerCgroupfs,
				client.CgroupManagerSystemd,
			} {
				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
					ID:            tr.ctrID,
					Command:       []string{"/busybox", "true"},
					Timeout:       10,
					CgroupManager: manager,
				})
				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(0), string(result.Stderr))
			}

			contents, err := fileContents(argsFile)
			Expect(err).To(Succeed())

			// The arguments of the create and exec calls.
			var calls [][]string

			for call := range strings.Lines(contents) {
				if fields := strings.Fields(call); slices.Contains(fields, "create") ||
					slices.Contains(fields, "exec") {
					calls = append(calls, fields)
				}
			}

			Expect(calls).To(HaveExactElements(
				And(ContainElement("create"), Not(ContainElement("--systemd-cgroup"))),
				And(ContainElement("exec"), Not(ContainElement("--systemd-cgroup"))),
				And(ContainElement("exec"), ContainElement("--systemd-cgroup")),
			))
		}, SpecTimeout(specTimeout))

		It("should not pass its transparent huge pages setting on", func(ctx SpecContext) {
			thpEnabled := func(pid string) string {
				status, err := os.ReadFile("/proc/" + pid + "/status")
				Expect(err).To(Succeed())

				thp := regexp.MustCompile(`THP_enabled:\s+(\d)`)

				// Empty on kernels before 5.0, which do not report it.
				match := thp.FindStringSubmatch(string(status))
				if match == nil {
					return ""
				}

				return match[1]
			}
			if thpEnabled("self") != "1" {
				Skip("transparent huge pages disabled or not reported for the tests")
			}

			runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

			// The server turns them off for itself only.
			Expect(thpEnabled(strconv.FormatUint(uint64(sut.PID()), 10))).To(Equal("0"))

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "grep", "THP_enabled", "/proc/self/status"},
				Timeout: 10,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0))
			Expect(string(result.Stdout)).To(MatchRegexp(`THP_enabled:\s+1`))
		}, SpecTimeout(specTimeout))

		for _, terminal := range []bool{true, false} {
			It(testName("should succeed without timeout", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

				result, err := sut.ExecSyncContainer(withoutDeadline(ctx), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  timeoutUnlimited,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())

				// The PID file of the exec is removed once it got read.
				Expect(execPIDFiles(tr.tmpDir)).To(BeEmpty())

				err = sut.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
					ID: tr.ctrID,
				})
				Expect(err).To(Succeed())

				Expect(fileContents(tr.logPath())).To(BeEmpty())
			}, SpecTimeout(specTimeout))

			It(testName("should succeed with timeout", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  10,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())
			}, SpecTimeout(specTimeout))

			It(testName("should set the correct exit code", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

				result, err := sut.ExecSyncContainer(withoutDeadline(ctx), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "invalid"},
					Timeout:  timeoutUnlimited,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(127))

				expectedStr := "invalid: applet not found"
				if terminal {
					expectedStr += "\r"
				}

				expectedStr += "\n"
				if terminal {
					Expect(result.Stdout).To(BeEquivalentTo(expectedStr))
					Expect(result.Stderr).To(BeEmpty())
				} else {
					Expect(result.Stdout).To(BeEmpty())
					Expect(result.Stderr).To(BeEquivalentTo(expectedStr))
				}
			}, SpecTimeout(specTimeout))

			It(testName("should timeout", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "20"})

				start := time.Now()
				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "sleep", "10"},
					Timeout:  3,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				Expect(time.Since(start)).To(BeNumerically("<", 5*time.Second))
				Expect(result).NotTo(BeNil())
				Expect(result.TimedOut).To(BeTrue())
				// The killed command reports no exit code of its own.
				Expect(result.ExitCode).To(BeNumerically("<", 0))
			}, SpecTimeout(specTimeout))

			It(
				testName("should not outlive the deadline of the context", terminal),
				func(ctx SpecContext) {
					runContainer(ctx, terminal, []string{"/busybox", "sleep", "20"})

					execCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
					defer cancel()

					// The server usually reports the timeout before the context
					// ends the call.
					result, err := sut.ExecSyncContainer(execCtx, &client.ExecSyncConfig{
						ID:       tr.ctrID,
						Command:  []string{"/busybox", "sleep", "30"},
						Terminal: terminal,
					})
					if err == nil {
						Expect(result.TimedOut).To(BeTrue())
					} else {
						Expect(err).To(MatchError(context.DeadlineExceeded))
					}

					// The command got killed with the deadline.
					Eventually(func(g Gomega) string {
						result, err := sut.ExecSyncContainer(
							ctx,
							&client.ExecSyncConfig{
								ID:      tr.ctrID,
								Command: []string{"/busybox", "ps"},
								Timeout: 10,
							},
						)
						g.Expect(err).To(Succeed())

						return string(result.Stdout)
					}, 4*time.Second).ShouldNot(ContainSubstring("sleep 30"))
				},
				SpecTimeout(specTimeout),
			)
		}
	})

	Describe("Attach", func() {
		It("should fail to attach to or resize an unknown container", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			socketPath := filepath.Join(tr.tmpDir, "attach")
			Expect(sut.AttachContainer(ctx, &client.AttachConfig{
				ID:         "unknown",
				SocketPath: socketPath,
			})).To(MatchError(client.ErrContainerNotFound))
			Expect(socketPath).NotTo(BeAnExistingFile())

			Expect(sut.SetWindowSizeContainer(ctx, &client.SetWindowSizeContainerConfig{
				ID:   "unknown",
				Size: &resize.TerminalSize{Width: 10, Height: 20},
			})).To(MatchError(client.ErrContainerNotFound))
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

				cfg := &client.AttachConfig{
					ID:                tr.ctrID,
					SocketPath:        filepath.Join(tr.tmpDir, "attach"),
					ContainerStdin:    true,
					StopAfterStdinEOF: !leaveStdinOpen,
					Streams: client.AttachStreams{
						Stdin: &client.In{ReadCloser: stdinReader},
					},
				}
				if pipe == "stdout" {
					cfg.Streams.Stdout = &client.Out{WriteCloser: stdWriter}
				} else {
					cfg.Streams.Stderr = &client.Out{WriteCloser: stdWriter}
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

			cfg := &client.AttachConfig{
				ID:                tr.ctrID,
				SocketPath:        filepath.Join(tr.tmpDir, "attach"),
				Tty:               terminal,
				StopAfterStdinEOF: stopAfterStdinEOF,
				ContainerStdin:    true,
				Streams: client.AttachStreams{
					Stdin:  &client.In{ReadCloser: stdinReader},
					Stdout: &client.Out{WriteCloser: stdoutWriter},
					Stderr: &client.Out{WriteCloser: stderrWriter},
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
						attachErr <- sut.AttachContainer(ctx, &client.AttachConfig{
							ID:             tr.ctrID,
							SocketPath:     socketPath,
							ContainerStdin: true,
							Streams: client.AttachStreams{
								Stdin:  &client.In{ReadCloser: stdinReader},
								Stdout: &client.Out{WriteCloser: stdoutWriter},
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
				attachErr <- sut.AttachContainer(attachCtx, &client.AttachConfig{
					ID:         tr.ctrID,
					SocketPath: filepath.Join(tr.tmpDir, "attach"),
					Tty:        true,
					Resize:     resizeEvents,
					Streams:    client.AttachStreams{Stdout: &client.Out{WriteCloser: stdoutWriter}},
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
				attachErr <- sut.AttachContainer(attachCtx, &client.AttachConfig{
					ID:                tr.ctrID,
					SocketPath:        filepath.Join(tr.tmpDir, "attach"),
					ContainerStdin:    true,
					StopAfterStdinEOF: true,
					Streams: client.AttachStreams{
						Stdin:  &client.In{ReadCloser: stdinReader},
						Stdout: &client.Out{WriteCloser: stdoutWriter},
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
				ctx,
				"/busybox cat; /busybox tail /dev/zero", false, true, "", true,
				func(g generate.Generator) {
					// Pin the swap limit as well, otherwise the cgroup swaps
					// on hosts with swap instead of being killed.
					const limit = 16 * 1024 * 1024
					g.SetLinuxResourcesMemoryLimit(limit)
					g.SetLinuxResourcesMemorySwap(limit)
				},
				nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(tr.oomExitPath, 10*time.Second).Should(BeAnExistingFile())
			Eventually(fileContents, 10*time.Second).
				WithArguments(tr.exitPath()).
				Should(Equal("137"))
		}, SpecTimeout(specTimeout))
	})

	Describe("Tracing", func() {
		const contribTracingPath = "../../../contrib/tracing/"

		BeforeEach(func() {
			// Tracing is a compile time feature of the server, only a build
			// with it can export spans. Check before Jaeger gets started.
			binary := conmonPath
			if binary == "" {
				binary = "conmonrs"
			}

			version, err := exec.Command(binary, "--version-json", "full").Output()
			Expect(err).NotTo(HaveOccurred())

			if !strings.Contains(string(version), "opentelemetry") {
				Skip("server built without the tracing feature")
			}

			cmd := exec.Command(contribTracingPath + "start")

			fmt.Fprintln(os.Stdout)
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})

		hasTraces := func() bool {
			// Jaeger v2 only serves the v3 query API, which requires a time range.
			now := time.Now().UTC()
			query := url.Values{}
			query.Set("query.service_name", "conmonrs")
			query.Set("query.start_time_min", now.Add(-time.Hour).Format(time.RFC3339))
			query.Set("query.start_time_max", now.Add(time.Minute).Format(time.RFC3339))

			req, err := http.NewRequestWithContext(
				context.Background(), http.MethodGet,
				"http://localhost:16686/api/v3/traces?"+query.Encode(), http.NoBody,
			)
			Expect(err).NotTo(HaveOccurred())

			resp, err := http.DefaultClient.Do(req)
			Expect(err).NotTo(HaveOccurred())

			defer resp.Body.Close()

			// No matching trace is reported as not found.
			if resp.StatusCode == http.StatusNotFound {
				return false
			}

			Expect(resp.StatusCode).To(Equal(http.StatusOK))

			// The API streams one result object per chunk.
			type traceData struct {
				Result struct {
					ResourceSpans []any `json:"resourceSpans"`
				} `json:"result"`
			}

			decoder := json.NewDecoder(resp.Body)

			for {
				traces := &traceData{}

				err := decoder.Decode(traces)
				if errors.Is(err, io.EOF) {
					return false
				}

				Expect(err).NotTo(HaveOccurred())

				if len(traces.Result.ResourceSpans) > 0 {
					return true
				}
			}
		}

		It("should succeed", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			tr.enableTracing = true

			sut = tr.configGivenEnv()

			tr.createContainer(ctx, sut, false)
			tr.startContainer()

			Eventually(hasTraces, 100*time.Second, time.Second).Should(BeTrue())
		}, SpecTimeout(3*time.Minute))

		AfterEach(func() {
			cmd := exec.Command(contribTracingPath + "stop")
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})
	})

	Describe("CreateNamespaces", func() {
		// The namespaces get bind mounted, which needs privileges.
		BeforeEach(func() {
			if unshare.IsRootless() {
				Skip("does not run rootless")
			}
		})

		It("should succeed without namespaces", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					PodID: podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())
		}, SpecTimeout(specTimeout))

		It("should fail without pod ID", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{},
			)
			Expect(err).NotTo(Succeed())
			Expect(response).To(BeNil())
		}, SpecTimeout(specTimeout))

		It("should succeed without user namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					PodID: podID,
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespacePID,
						client.NamespaceUTS,
					},
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())

			// Only the requested namespaces are created, in request order.
			Expect(response.Namespaces).To(HaveLen(4))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUTS))

			const basePath = "/var/run/"

			for i, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
				Expect(filepath.Base(ns.Path)).To(Equal(podID))

				switch i {
				case 0:
					Expect(ns.Path).To(ContainSubstring(basePath + "ipcns/"))
				case 1:
					Expect(ns.Path).To(ContainSubstring(basePath + "netns/"))
				case 2:
					Expect(ns.Path).To(ContainSubstring(basePath + "pidns/"))
				case 3:
					Expect(ns.Path).To(ContainSubstring(basePath + "utsns/"))
				}
			}

			// The user namespace was not requested and is not bound.
			Expect(filepath.Join(basePath, "userns", podID)).NotTo(BeAnExistingFile())
		}, SpecTimeout(specTimeout))

		It("should not keep a pause process without PID namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespaceUTS,
					},
					BasePath: basePath,
					PodID:    podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())
			Expect(response.Namespaces).To(HaveLen(3))

			// The bind mounts keep the namespaces alive on their own.
			for _, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
			}

			// Without a PID namespace no pause process stays behind.
			Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).NotTo(BeAnExistingFile())
			Expect(pauseProcessRunning(podID)).To(BeFalse())
		}, SpecTimeout(specTimeout))

		It(
			"should not keep a pause process with user namespace and without PID namespace",
			func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfig(false)
				sut = tr.configGivenEnv()

				basePath := MustTempDir("ns-test-")
				// Runs after the server shutdown unmounted the namespaces.
				DeferCleanup(os.RemoveAll, basePath)

				podID := uuid.New().String()

				uids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}
				gids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

				response, err := sut.CreateNamespaces(
					ctx,
					&client.CreateNamespacesConfig{
						Namespaces: []client.Namespace{
							client.NamespaceIPC,
							client.NamespaceNet,
							client.NamespaceUTS,
							client.NamespaceUser,
							// Duplicates are reported once.
							client.NamespaceNet,
						},
						IDMappings: idtools.NewIDMappingsFromMaps(uids, gids),
						BasePath:   basePath,
						PodID:      podID,
					},
				)
				Expect(err).To(Succeed())
				Expect(response).NotTo(BeNil())
				Expect(response.Namespaces).To(HaveLen(4))
				Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUser))

				// The namespaces are bound on the host and outlive the pause process.
				for _, ns := range response.Namespaces {
					expectNamespaceFile(ns.Path)
				}

				Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).NotTo(BeAnExistingFile())
				Expect(pauseProcessRunning(podID)).To(BeFalse())
			},
			SpecTimeout(specTimeout),
		)

		It("should succeed with user namespace and custom base path", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			uids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}
			gids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespacePID,
						client.NamespaceUTS,
						client.NamespaceUser,
					},
					IDMappings: idtools.NewIDMappingsFromMaps(uids, gids),
					BasePath:   basePath,
					PodID:      podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())

			Expect(response.Namespaces).To(HaveLen(5))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUTS))
			Expect(response.Namespaces[4].Type).To(Equal(client.NamespaceUser))

			for _, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
			}

			// The PID namespace needs the pause process to stay alive.
			Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).To(BeAnExistingFile())
			Expect(pauseProcessRunning(podID)).To(BeTrue())
		}, SpecTimeout(specTimeout))

		It("should reap orphans in the pod PID namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{client.NamespacePID},
					BasePath:   basePath,
					PodID:      podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(1))

			pidFile, err := os.ReadFile(filepath.Join(basePath, "conmonrs", podID+".pid"))
			Expect(err).To(Succeed())
			pausePID, err := strconv.Atoi(strings.TrimSpace(string(pidFile)))
			Expect(err).To(Succeed())

			// The shell exits right away, so its background child gets
			// reparented to the pause process, the init of the PID namespace.
			//nolint:gosec // test helper with controlled inputs
			cmd := exec.CommandContext(
				ctx,
				"nsenter", "--pid="+response.Namespaces[0].Path, "--",
				"sh", "-c", "sleep 1 & exit 0",
			)
			Expect(cmd.Run()).To(Succeed())
			Expect(childrenOf(pausePID)).NotTo(BeEmpty())

			// Once the orphan exited, it does not stay a zombie.
			Eventually(func() []int { return childrenOf(pausePID) }).
				WithTimeout(10 * time.Second).
				Should(BeEmpty())
		}, SpecTimeout(specTimeout))

		It("should fail for another pod or configuration", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			cfg := client.CreateNamespacesConfig{
				Namespaces: []client.Namespace{client.NamespaceIPC, client.NamespaceUTS},
				BasePath:   basePath,
				PodID:      uuid.New().String(),
			}
			response, err := sut.CreateNamespaces(ctx, &cfg)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(2))

			// The same request is answered with the same namespaces, in any order.
			same := cfg
			same.Namespaces = []client.Namespace{client.NamespaceUTS, client.NamespaceIPC}
			again, err := sut.CreateNamespaces(ctx, &same)
			Expect(err).To(Succeed())
			Expect(again.Namespaces).To(Equal(response.Namespaces))

			otherPod := cfg
			otherPod.PodID = uuid.New().String()
			_, err = sut.CreateNamespaces(ctx, &otherPod)
			Expect(err).NotTo(Succeed())

			otherNamespaces := cfg
			otherNamespaces.Namespaces = []client.Namespace{client.NamespaceNet}
			_, err = sut.CreateNamespaces(ctx, &otherNamespaces)
			Expect(err).NotTo(Succeed())

			otherBasePath := cfg
			otherBasePath.BasePath = MustTempDir("ns-test-")
			DeferCleanup(os.RemoveAll, otherBasePath.BasePath)
			_, err = sut.CreateNamespaces(ctx, &otherBasePath)
			Expect(err).NotTo(Succeed())
		}, SpecTimeout(specTimeout))

		It("should create the namespaces once for concurrent calls", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()
			cfg := &client.CreateNamespacesConfig{
				Namespaces: []client.Namespace{
					client.NamespaceIPC,
					client.NamespacePID,
					client.NamespaceUTS,
				},
				BasePath: basePath,
				PodID:    podID,
			}

			const calls = 10

			responses := make([]*client.CreateNamespacesResponse, calls)

			var wg sync.WaitGroup
			for i := range calls {
				wg.Go(func() {
					defer GinkgoRecover()

					response, err := sut.CreateNamespaces(ctx, cfg)
					Expect(err).To(Succeed())

					responses[i] = response
				})
			}

			wg.Wait()

			for _, response := range responses {
				Expect(response.Namespaces).To(Equal(responses[0].Namespaces))
			}

			for _, ns := range responses[0].Namespaces {
				expectNamespaceFile(ns.Path)
			}

			Expect(pauseProcessCount(podID)).To(Equal(1))
		}, SpecTimeout(specTimeout))

		It("should fail with user namespace without mappings", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceUser,
					},
				},
			)
			Expect(err).NotTo(Succeed())
			Expect(errors.Is(err, client.ErrMissingIDMappings)).To(BeTrue())
			Expect(response).To(BeNil())
		}, SpecTimeout(specTimeout))
	})
})

var _ = Describe("JSONLogger", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	BeforeEach(func() {
		tr = nil
		sut = nil
	})

	AfterEach(func() {
		cleanupSpec(tr, sut)
	})

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log in JSON format", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)

				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers = []client.ContainerLogDriver{{
					Type: client.LogDriverTypeJSONLogger,
					Path: tr.logPath(),
				}}
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// The schema is documented in usage.md.
				type entry struct {
					Timestamp uint64 `json:"timestamp"`
					Pipe      string `json:"pipe"`
					Message   string `json:"message"`
					Partial   bool   `json:"partial"`
				}

				var entries []entry

				Eventually(func(g Gomega) []entry {
					entries = nil

					contents, err := fileContents(tr.logPath())
					g.Expect(err).To(Succeed())

					for line := range strings.Lines(contents) {
						var e entry
						g.Expect(json.Unmarshal([]byte(line), &e)).To(Succeed(), "line %q", line)
						entries = append(entries, e)
					}

					return entries
				}, 10*time.Second).Should(HaveLen(2))

				for i, message := range []string{"hello", "world"} {
					Expect(entries[i].Timestamp).NotTo(BeZero())
					Expect(entries[i].Pipe).To(Equal("stdout"))
					// A terminal ends the lines with "\r\n", which is stripped as well.
					Expect(entries[i].Message).To(Equal(message))
					Expect(entries[i].Partial).To(BeFalse())
				}
			}, SpecTimeout(specTimeout))
		}
	})
})

var _ = Describe("JournaldLogger", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	BeforeEach(func() {
		tr = nil
		sut = nil
	})

	AfterEach(func() {
		cleanupSpec(tr, sut)
	})

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log to journald", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				// Unique markers, other specs may log to the journal at the same time.
				marker := uuid.New().String()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{
						"/busybox",
						"sh",
						"-c",
						"echo hello " + marker + " && echo foo " + marker + " >&2",
					},
					nil,
				)

				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers = []client.ContainerLogDriver{{Type: client.LogDriverTypeJournald}}
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// Verify the journal logs, they arrive asynchronously. The
				// documented short ID field finds both lines.
				journal := func(extraArgs ...string) func(g Gomega) string {
					return func(g Gomega) string {
						args := append(
							[]string{"--since", "-1min", "CONTAINER_ID=" + tr.ctrID[:12]},
							extraArgs...)
						cmd := exec.Command("journalctl", args...)
						stdout := strings.Builder{}
						cmd.Stdout = &stdout
						g.Expect(cmd.Run()).NotTo(HaveOccurred())

						return stdout.String()
					}
				}
				Eventually(journal(), 10*time.Second).Should(And(
					ContainSubstring("hello "+marker),
					ContainSubstring("foo "+marker),
				))

				// Stderr goes out at error priority, stdout does not. A
				// terminal merges both into stdout.
				if !terminal {
					Eventually(journal("--priority", "err"), 10*time.Second).Should(And(
						Not(ContainSubstring("hello "+marker)),
						ContainSubstring("foo "+marker),
					))
				}
			}, SpecTimeout(specTimeout))
		}

		It("should write the container name, tag and labels", func(ctx SpecContext) {
			tr = newTestRunner()
			marker := uuid.New().String()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "echo", "hello " + marker},
				nil,
			)

			sut = tr.configGivenEnv()
			Expect(sut.SupportsFeature(client.FeatureJournaldFields)).To(BeTrue())

			cfg := tr.defaultConfig(false)
			cfg.LogDrivers = []client.ContainerLogDriver{{
				Type:          client.LogDriverTypeJournald,
				ContainerName: "name-" + marker,
				Tag:           "tag-" + marker,
				Labels:        map[string]string{"CONMON_TEST_LABEL": "label-" + marker},
			}}
			tr.createContainerWithConfig(ctx, sut, cfg)
			tr.startContainer()

			// The matches of different fields all have to hold, so the line
			// only shows up with every field set. The tag names the
			// identifier as well.
			args := []string{
				"--since", "-1min",
				"CONTAINER_ID=" + tr.ctrID[:12],
				"CONTAINER_NAME=name-" + marker,
				"CONTAINER_TAG=tag-" + marker,
				"SYSLOG_IDENTIFIER=tag-" + marker,
				"CONMON_TEST_LABEL=label-" + marker,
			}
			Eventually(func(g Gomega) string {
				cmd := exec.CommandContext(ctx, "journalctl", args...)
				stdout := strings.Builder{}
				cmd.Stdout = &stdout
				g.Expect(cmd.Run()).To(Succeed())

				return stdout.String()
			}, 10*time.Second).Should(ContainSubstring("hello " + marker))
		}, SpecTimeout(specTimeout))
	})
})

var _ = Describe("StreamingServer", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	BeforeEach(func() {
		tr = nil
		sut = nil
	})

	AfterEach(func() {
		cleanupSpec(tr, sut)
	})

	// runContainer starts a server with a started container which runs the
	// process.
	runContainer := func(ctx context.Context, terminal bool, processArgs []string) {
		tr = newTestRunner()
		tr.createRuntimeConfigWithProcessArgs(terminal, processArgs, nil)
		sut = tr.configGivenEnv()
		tr.createContainer(ctx, sut, terminal)
		tr.startContainer()
	}

	Describe("ServePortForwardContainer", func() {
		It("should forward concurrent connections", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

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

		It("should forward connections through a WebSocket tunnel", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

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
					tr = newTestRunner()
					tr.createRuntimeConfig(false)
					sut = tr.configGivenEnv()

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
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

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

		It("should reject a missing network namespace path", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			// Also without a config.
			for _, cfg := range []*client.ServePortForwardContainerConfig{nil, {}} {
				result, err := sut.ServePortForwardContainer(ctx, cfg)
				Expect(err).To(MatchError(client.ErrMissingNetNsPath))
				Expect(result).To(BeNil())
			}
		}, SpecTimeout(specTimeout))

		It("should report a closed port on the error stream", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

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
					testName("should fail if container does not exist", terminal),
					func(ctx SpecContext) {
						tr = newTestRunner()
						tr.createRuntimeConfigWithProcessArgs(
							terminal,
							[]string{"/busybox", "sleep", "10"},
							nil,
						)
						sut = tr.configGivenEnv()

						result, err := sut.ServeExecContainer(
							ctx,
							&client.ServeExecContainerConfig{
								ID: "wrong",
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

						Expect(err).To(MatchError(client.ErrContainerNotFound))
						Expect(result).To(BeNil())
					},
					SpecTimeout(specTimeout),
				)

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

			It(
				testName("should fail if container does not exist", terminal),
				func(ctx SpecContext) {
					tr = newTestRunner()
					tr.createRuntimeConfigWithProcessArgs(
						terminal,
						[]string{"/busybox", "sleep", "10"},
						nil,
					)
					sut = tr.configGivenEnv()

					result, err := sut.ServeAttachContainer(
						ctx,
						&client.ServeAttachContainerConfig{
							ID: "wrong",
						},
					)

					Expect(err).To(MatchError(client.ErrContainerNotFound))
					Expect(result).To(BeNil())
				},
				SpecTimeout(specTimeout),
			)
		})
	}

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
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

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
