package client_test

import (
	"os"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-tools/generate"
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
					if unshare.IsRootless() {
						Skip("does not run rootless")
					}

					tr = newTestRunner()
					tr.createRuntimeConfigWithProcessArgs(
						terminal,
						[]string{"/busybox", "tail", "/dev/zero"},
						func(g generate.Generator) {
							// Pin the swap as well, otherwise the cgroup swaps on
							// hosts with swap instead of being killed. Enough
							// for the init of runc, a Go program, tail grows
							// beyond any limit.
							const limit = 32 * 1024 * 1024
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
