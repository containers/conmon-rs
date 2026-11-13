package client_test

import (
	"os"
	"path/filepath"
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
	newServer := serverStarter(&tr, &sut)
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
				testName("should catch out of memory (oom) events", terminal),
				func(ctx SpecContext) {
					skipWithoutOOMDetection()

					tr = newTestRunner()
					tr.createRuntimeConfigWithProcessArgs(
						terminal, []string{"/busybox", "tail", "/dev/zero"}, oomSpec,
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
	})

	Describe("ReopenLogContainer", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should continue the log in a new file", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sh", "-c", countingLoop})

				Eventually(
					func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
					10*time.Second,
				).
					Should(atLeastLines(3))

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
				Expect(oldLines).To(atLeastLines(3))

				Eventually(
					func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
					10*time.Second,
				).
					Should(atLeastLines(3))

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
			newServer()

			Expect(sut.ReopenLogContainer(ctx, &client.ReopenLogContainerConfig{
				ID: "unknown",
			})).To(MatchError(client.ErrContainerNotFound))

			// The failed request keeps the server usable.
			_, err := sut.Version(ctx)
			Expect(err).To(Succeed())
		}, SpecTimeout(specTimeout))
	})

	Describe("CreateContainer with RestorePath", func() {
		for _, terminal := range []bool{false, true} {
			It(
				testName("should restore a container from a checkpoint", terminal),
				func(ctx SpecContext) {
					skipWithoutCheckpointSupport()

					runContainer(ctx, terminal, []string{"/busybox", "sh", "-c", countingLoop})
					Expect(sut.SupportsFeature(client.FeatureRestore)).To(BeTrue())

					Eventually(
						func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
						10*time.Second,
					).
						Should(atLeastLines(3))

					// The checkpoint stops the container, like CRI-O's does, so the
					// server writes its exit file, which tells that its log is
					// complete. The runtime keeps the stopped container until it
					// gets deleted.
					imagePath := MustDirInTempDir(tr.tmpDir, "checkpoint")
					Expect(tr.rr.RunCommand(
						"checkpoint", "--image-path", imagePath, "--work-path", imagePath, tr.ctrID,
					)).To(Succeed())
					// criu kills the process once it got dumped, like a SIGKILL.
					Eventually(fileContents, 10*time.Second).
						WithArguments(tr.exitPath()).
						Should(Equal("137"))
					tr.expectContainerStoppedOrGone()
					Expect(tr.rr.RunCommand("delete", "-f", tr.ctrID)).To(Succeed())

					// The restore logs to the same file, like CRI-O restores to the
					// log path of the container, and the server appends to it. The
					// exit file of the checkpointed container stays, so the restored
					// one gets its own.
					oldLines := logLineNumbers(Default, tr.logPath())
					Expect(oldLines).To(atLeastLines(3))
					Expect(oldLines[0]).To(Equal(0))

					cfg := tr.defaultConfig(terminal)
					cfg.RestorePath = imagePath
					cfg.ExitPaths = []string{filepath.Join(tr.tmpDir, "exit-restored")}
					resp, err := sut.CreateContainer(ctx, cfg)
					Expect(err).To(Succeed())
					Expect(resp.PID).NotTo(BeEquivalentTo(0))

					// The restored container runs without a start, and the server
					// monitors it: it logs its output and writes its exit file.
					Eventually(func() error {
						return tr.rr.RunCommandCheckOutput("running", "list")
					}, 10*time.Second).Should(Succeed())
					Eventually(
						func(g Gomega) []int { return logLineNumbers(g, tr.logPath()) },
						10*time.Second,
					).
						Should(atLeastLines(len(oldLines) + 3))

					// The same process continues where the checkpoint stopped it: the
					// lines of the restored container follow the ones from before
					// the checkpoint in the same file, without a gap or a repeat.
					lines := logLineNumbers(Default, tr.logPath())
					for i := 1; i < len(lines); i++ {
						Expect(lines[i]).To(Equal(lines[i-1]+1), "lines %v", lines)
					}

					Expect(tr.rr.RunCommand("kill", tr.ctrID, "KILL")).To(Succeed())
					// Killed by SIGKILL, like a shell reports it.
					Eventually(fileContents, 10*time.Second).
						WithArguments(cfg.ExitPaths[0]).
						Should(Equal("137"))
				},
				SpecTimeout(specTimeout),
			)
		}

		It("should fail for a checkpoint the runtime rejects", func(ctx SpecContext) {
			newServer()
			Expect(sut.SupportsFeature(client.FeatureRestore)).To(BeTrue())

			// An empty directory holds no checkpoint. The runtime fails
			// before criu restores anything, so this needs neither criu nor
			// root, with runc and with a crun built without CRIU alike.
			cfg := tr.defaultConfig(false)
			cfg.RestorePath = MustDirInTempDir(tr.tmpDir, "empty")
			_, err := sut.CreateContainer(ctx, cfg)
			// The error carries the stderr of the runtime, the message itself
			// belongs to the runtime.
			Expect(err).To(MatchError(MatchRegexp(`runtime exited with status 1: \S`)))

			// The server registered nothing: the runtime does not list the
			// container and the server writes no exit file for it.
			Expect(tr.rr.RunCommandCheckOutput(tr.ctrID, "list")).To(MatchError(errNoMatch))
			Expect(tr.exitPath()).NotTo(BeAnExistingFile())

			// The failed request keeps the server usable.
			_, err = sut.Version(ctx)
			Expect(err).To(Succeed())
		}, SpecTimeout(specTimeout))

		It("should fail for a relative checkpoint path", func(ctx SpecContext) {
			newServer()

			cfg := tr.defaultConfig(false)
			cfg.RestorePath = "checkpoint"
			_, err := sut.CreateContainer(ctx, cfg)
			Expect(err).To(MatchError(
				ContainSubstring(`restore path "checkpoint" is not absolute`),
			))

			// The failed request keeps the server usable.
			_, err = sut.Version(ctx)
			Expect(err).To(Succeed())
		}, SpecTimeout(specTimeout))
	})
})
