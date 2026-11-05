package client_test

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)
	runContainer := containerRunner(&tr, &sut)

	Describe("New", func() {
		It("should restore from running server", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "60"})

			sut2 := tr.configGivenEnv()
			Expect(sut2.PID()).To(Equal(sut.PID()))

			// The container of the server can be used through the new
			// client.
			result, err := sut2.ExecSyncContainer(ctx, &client.ExecSyncConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "echo", "hello"},
				Timeout: 10,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeZero())
			Expect(string(result.Stdout)).To(Equal("hello\n"))
			Expect(sut2.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
				ID: tr.ctrID,
			})).To(Succeed())

			// Only the client goes away.
			Expect(sut2.Close()).To(Succeed())

			_, err = sut.Version(ctx, &client.VersionConfig{})
			Expect(err).To(Succeed())

			// Another one stops the server, which stops the container.
			sut3 := tr.configGivenEnv()
			DeferCleanup(sut3.Close)
			DeferCleanup(sut.Close)
			Expect(sut3.Shutdown()).To(Succeed())

			sut = nil

			tr.expectContainerStoppedOrGone()
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
		// files. Its shutdown takes up to 9.5 seconds, after SIGTERM
		// including up to 5 for the containers to exit on it.
		expectServerGone := func(pid uint32) {
			Eventually(func() error {
				return syscall.Kill(int(pid), 0)
			}, 10*time.Second).Should(MatchError(syscall.ESRCH))
			// The server PID file is removed by New already, the "pidfile"
			// in the run directory belongs to the container of the bundle.
			Expect(filepath.Join(tr.tmpDir, "conmon.sock")).NotTo(BeAnExistingFile())
			Expect(filepath.Join(tr.tmpDir, "conmon-fd.sock")).NotTo(BeAnExistingFile())

			tr.expectContainerStoppedOrGone()
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
				// Every feature of the server has a constant of the client.
				Expect(version.Features).To(ConsistOf(
					client.FeatureSdNotify,
					client.FeaturePortForward,
					client.FeatureIdleTimeout,
					client.FeatureStreamingProtocols,
					client.FeatureJournaldFields,
				))

				if verbose {
					Expect(version.CargoTree).NotTo(BeEmpty())
				} else {
					Expect(version.CargoTree).To(BeEmpty())
				}
			}, SpecTimeout(specTimeout))
		}
	})
})
