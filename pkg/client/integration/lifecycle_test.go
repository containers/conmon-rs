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
	"github.com/opencontainers/runtime-spec/specs-go"
	"github.com/opencontainers/runtime-tools/generate"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)
	newServer := serverStarter(&tr, &sut)
	runContainer := containerRunner(&tr, &sut)

	Describe("New", func() {
		It("should restore from running server", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "60"})

			sut2 := tr.configGivenEnv()
			Expect(sut2.PID()).To(Equal(sut.PID()))

			// The container of the server can be used through the new
			// client.
			result, err := sut2.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "echo", "hello"},
				Timeout: 10 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeZero())
			Expect(string(result.Stdout)).To(Equal("hello\n"))
			Expect(sut2.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
				ID: tr.ctrID,
			})).To(Succeed())

			// Only the client goes away.
			Expect(sut2.Close()).To(Succeed())

			_, err = sut.Version(ctx)
			Expect(err).To(Succeed())

			// Another one stops the server, which stops the container.
			sut3 := tr.configGivenEnv()
			DeferCleanup(sut3.Close)
			DeferCleanup(sut.Close)
			Expect(sut3.Shutdown(ctx)).To(Succeed())

			sut = nil

			tr.expectContainerStoppedOrGone()
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
			newServer()

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
			// an answer. The call still uses the connection of the old
			// server if the client did not notice its end yet, and dials
			// the socket, which nobody listens on, otherwise.
			start := time.Now()
			_, err := old.Version(ctx)
			Expect(err).To(Or(
				MatchError(client.ErrServerDisconnected),
				MatchError(client.ErrServerUnreachable),
			))
			Expect(time.Since(start)).To(BeNumerically("<", time.Second))

			// That call marked the connection as broken at the latest, the
			// next one dials the socket.
			_, err = old.Version(ctx)
			Expect(err).To(MatchError(client.ErrServerUnreachable))

			// The cleanup shuts down the new server.
			sut = tr.configGivenEnv()
			Expect(sut.PID()).NotTo(Equal(oldPID))

			oldInfo, err := os.Lstat(oldSocket)
			Expect(err).NotTo(HaveOccurred())
			newInfo, err := os.Lstat(socket)
			Expect(err).NotTo(HaveOccurred())
			Expect(os.SameFile(oldInfo, newInfo)).To(BeFalse(), "stale socket not replaced")

			_, err = sut.Version(ctx)
			Expect(err).To(Succeed())

			// The client of the old server connects to the new one with its
			// next call, and records its PID.
			_, err = old.Version(ctx)
			Expect(err).To(Succeed())
			Expect(old.PID()).To(Equal(sut.PID()))

			Expect(old.Close()).To(Succeed())
			_, err = old.Version(ctx)
			Expect(err).To(MatchError(client.ErrClientClosed))
		}, SpecTimeout(specTimeout))
	})

	Describe("Shutdown", func() {
		// startServer starts a server with a created container and returns
		// its PID.
		startServer := func(ctx context.Context, terminal bool) uint32 {
			tr = newTestRunner()
			tr.createRuntimeConfig(terminal)
			sut = tr.configGivenEnv()
			tr.createContainer(ctx, sut, terminal)

			Expect(filepath.Join(tr.tmpDir, "conmon.sock")).To(BeAnExistingFile())

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

			tr.expectContainerStoppedOrGone()
		}

		for _, terminal := range []bool{true, false} {
			It(
				testName("should kill created children if being killed", terminal),
				func(ctx SpecContext) {
					pid := startServer(ctx, terminal)

					// Shutdown should kill the container and clean up
					Expect(sut.Shutdown(ctx)).To(Succeed())
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

		// startTerminable starts a server with a started container which
		// runs the process, and returns the PID of the server and a function
		// which reads its output. The container shares the PID namespace of
		// the host, so that its process gets the default action of SIGTERM,
		// which the init of a PID namespace of its own ignores. Its /proc is
		// a bind mount of the one of the host, a rootless runtime cannot
		// mount a new one for that namespace.
		startTerminable := func(
			ctx context.Context, processArgs []string,
		) (uint32, func() (string, error)) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(false, processArgs, func(g generate.Generator) {
				Expect(g.RemoveLinuxNamespace(string(specs.PIDNamespace))).To(Succeed())
				g.RemoveMount("/proc")
				g.AddMount(specs.Mount{
					Destination: "/proc",
					Type:        "bind",
					Source:      "/proc",
					Options:     []string{"rbind"},
				})
			})

			output, readOutput := serverOutput(tr.tmpDir)
			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.Stdout = output
				cfg.Stderr = output
			})
			tr.createContainer(ctx, sut, false)
			tr.startContainer()

			return sut.PID(), readOutput
		}

		It("should terminate the containers on SIGTERM", func(ctx SpecContext) {
			pid, _ := startTerminable(ctx, []string{"/busybox", "sleep", "1000"})
			Expect(syscall.Kill(int(pid), syscall.SIGTERM)).To(Succeed())

			// The hooks must not look at the server, which goes away by
			// itself, only the client is left to release.
			DeferCleanup(sut.Close)

			sut = nil

			expectServerGone(pid)

			// The container exited on the SIGTERM the server forwarded, like
			// a shell reports it, before the server stopped.
			Expect(fileContents(tr.exitPath())).To(Equal("143"))
		}, SpecTimeout(specTimeout))

		It("should kill the containers on a second SIGTERM", func(ctx SpecContext) {
			// The shell ignores SIGTERM before it runs sleep, which keeps
			// ignoring it.
			sleep := []string{"/busybox", "sleep", "1001"}
			pid, readOutput := startTerminable(ctx, []string{
				"/busybox", "sh", "-c", "trap '' TERM; exec " + strings.Join(sleep, " "),
			})
			Eventually(pidsWithCmdline, 10*time.Second).
				WithArguments(sleep).
				ShouldNot(BeEmpty())

			DeferCleanup(sut.Close)

			sut = nil

			Expect(syscall.Kill(int(pid), syscall.SIGTERM)).To(Succeed())
			// A signal which arrives before the server took the first one
			// would count as the same.
			Eventually(readOutput, 10*time.Second).Should(ContainSubstring("Received SIGTERM"))

			start := time.Now()

			Expect(syscall.Kill(int(pid), syscall.SIGTERM)).To(Succeed())

			// The shutdown does not wait for the containers after the second
			// signal. The log line below proves that the second signal ended
			// the wait, the grace would kill with 137 as well.
			Eventually(func() error {
				return syscall.Kill(int(pid), 0)
			}, 4*time.Second).Should(MatchError(syscall.ESRCH))
			Expect(time.Since(start)).To(BeNumerically("<", 4*time.Second))
			expectServerGone(pid)
			Expect(readOutput()).To(ContainSubstring("Received another SIGTERM"))
			Expect(fileContents(tr.exitPath())).To(Equal("137"))
		}, SpecTimeout(specTimeout))

		It("should leave created children behind if being killed", func(ctx SpecContext) {
			pid := startServer(ctx, false)

			// Closing the client of the killed server is safe. A failed kill
			// leaves the server to the cleanup.
			DeferCleanup(sut.Close)
			Expect(sut.Kill(ctx)).To(Succeed())

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
		It("should succeed", func(ctx SpecContext) {
			newServer()

			version, err := sut.Version(ctx)
			Expect(err).To(Succeed())

			Expect(version.ProcessID).To(Equal(sut.PID()))
			Expect(version.Version).NotTo(BeEmpty())
			Expect(version.Commit).NotTo(BeEmpty())
			Expect(version.BuildDate).NotTo(BeEmpty())
			Expect(version.Target).NotTo(BeEmpty())
			Expect(version.RustVersion).NotTo(BeEmpty())
			Expect(version.CargoVersion).NotTo(BeEmpty())
			// Every feature of the server has a constant of the client.
			Expect(version.Features).To(ConsistOf(
				client.FeaturePortForward,
				client.FeatureIdleTimeout,
				client.FeatureStreamingProtocols,
				client.FeatureRestore,
			))
		}, SpecTimeout(specTimeout))
	})
})
