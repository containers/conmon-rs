package client_test

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
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
	newServer := serverStarter(&tr, &sut)
	runContainer := containerRunner(&tr, &sut)

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

						tr.execEcho(execCtx, sut, terminal, i)
					}(i)
				}

				wg.Wait()

				expectNoResourceLeak(sut.PID(), resources)
			}, SpecTimeout(specTimeout))

			It(testName("should not leak memory", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "600"})

				pid := sut.PID()
				execCtx := withoutDeadline(ctx)
				execEcho := func(i int) { tr.execEcho(execCtx, sut, terminal, i) }

				// The first execs grow the heap of the allocator once, which
				// on AArch64 can take a whole transparent huge page of 2 MiB.
				// That is no leak, the measurement starts after them.
				const warmUps = 5
				for i := range warmUps {
					execEcho(i)
				}

				// The private anonymous memory of the server, which a leak
				// grows, unlike the mapped binary and libraries.
				anonBefore := anonymousKBGivenPID(pid)
				GinkgoWriter.Printf("Anonymous before: %d\n", anonBefore)
				logMemoryDetails(pid, "before")
				mappingsBefore := mappingRSS(pid)
				resources := resourcesOf(pid)

				for i := range 25 {
					execEcho(warmUps + i)
				}

				anonAfter := anonymousKBGivenPID(pid)
				growth := anonAfter - anonBefore
				GinkgoWriter.Printf("Anonymous after: %d\n", anonAfter)
				logMemoryDetails(pid, "after")
				logGrownMappings(mappingsBefore, mappingRSS(pid))
				GinkgoWriter.Printf("Anonymous diff: %d\n", growth)
				Expect(growth).To(BeNumerically("<", maxAnonGrowthKB), "Anonymous growth in kB")

				expectNoResourceLeak(pid, resources)
			}, SpecTimeout(specTimeout))
		}
	})

	Describe("ExecSyncContainer", func() {
		It("should return a large output of both streams", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "600"})

			const size = 1 << 20

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID: tr.ctrID,
				Command: []string{
					"/busybox",
					"sh",
					"-c",
					fmt.Sprintf(
						"/busybox head -c %d /dev/zero && /busybox head -c %d /dev/zero >&2",
						size, size,
					),
				},
				Timeout: 30 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0), string(result.Stderr))
			Expect(result.TimedOut).To(BeFalse())
			Expect(result.Stdout).To(HaveLen(size))
			Expect(result.Stderr).To(HaveLen(size))
		}, SpecTimeout(specTimeout))

		It("should cut an output beyond the limit", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "600"})

			// The server keeps less than 16 MiB of both streams together,
			// stops collecting a stream there and drops the rest, so the
			// process still runs to its end.
			const limit = 16 << 20

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID: tr.ctrID,
				Command: []string{
					"/busybox",
					"sh",
					"-c",
					fmt.Sprintf("/busybox head -c %d /dev/zero && echo -n done >&2", limit+(1<<20)),
				},
				Timeout: 30 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0), string(result.Stderr))
			Expect(result.TimedOut).To(BeFalse())
			Expect(len(result.Stdout)).To(And(
				BeNumerically("<", limit),
				BeNumerically(">", limit-(1<<20)),
			))
			Expect(string(result.Stderr)).To(Equal("done"))
		}, SpecTimeout(specTimeout))

		It("should not start a command in an unknown container", func(ctx SpecContext) {
			newServer()

			// The runtime fails, which the result tells, not an error.
			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID:      "unknown",
				Command: []string{"/busybox", "true"},
				Timeout: 10 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(Equal(client.ExecExitCodeNotStarted))
			Expect(result.TimedOut).To(BeFalse())
			Expect(result.Stderr).NotTo(BeEmpty())
		}, SpecTimeout(specTimeout))

		It("should not pass a command with a leading dash as an option", func(ctx SpecContext) {
			runContainer(ctx, false, []string{"/busybox", "sleep", "10"})

			exec := func(command string) *client.ExecSyncContainerResult {
				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
					ID:      tr.ctrID,
					Command: []string{command},
					Timeout: 10 * time.Second,
				})
				Expect(err).To(Succeed())
				GinkgoWriter.Printf("%s: exit code %d, stdout %q, stderr %q\n",
					command, result.ExitCode, result.Stdout, result.Stderr)

				return result
			}

			// The runtime takes it for the executable, which does not exist,
			// rather than for its option, which would print its version.
			Expect(exec("/does-not-exist").ExitCode).To(Equal(client.ExecExitCodeNotStarted))

			result := exec("--version")
			Expect(result.ExitCode).To(Equal(client.ExecExitCodeNotStarted))
			Expect(string(result.Stderr)).To(ContainSubstring("--version"))
			Expect(string(result.Stdout) + string(result.Stderr)).
				NotTo(ContainSubstring("commit:"))
		}, SpecTimeout(specTimeout))

		It("should pass the cgroup manager of the client to the runtime", func(ctx SpecContext) {
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
				cfg.CgroupManager = client.CgroupManagerCgroupfs
			})
			tr.createContainer(ctx, sut, false)
			tr.startContainer()

			// A second client of the same server sends its own one, it is a
			// setting of the client.
			systemd := tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.Runtime = wrapper
				cfg.CgroupManager = client.CgroupManagerSystemd
			})
			DeferCleanup(systemd.Close)

			for _, c := range []*client.ConmonClient{sut, systemd} {
				result, err := c.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
					ID:      tr.ctrID,
					Command: []string{"/busybox", "true"},
					Timeout: 10 * time.Second,
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

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "grep", "THP_enabled", "/proc/self/status"},
				Timeout: 10 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0))
			Expect(string(result.Stdout)).To(MatchRegexp(`THP_enabled:\s+1`))
		}, SpecTimeout(specTimeout))

		It("should not pass its OOM score adjustment on", func(ctx SpecContext) {
			oomScoreAdj := func(pid string) string {
				score, err := os.ReadFile("/proc/" + pid + "/oom_score_adj")
				Expect(err).To(Succeed())

				return strings.TrimSpace(string(score))
			}
			own := oomScoreAdj("self")

			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "sleep", "10"},
				func(g generate.Generator) {
					// Without a value of its own the container keeps the
					// one of the runtime.
					g.Config.Process.OOMScoreAdj = nil
				},
			)
			sut = tr.configGivenEnv()
			resp, err := sut.CreateContainer(ctx, tr.defaultConfig(false))
			Expect(err).To(Succeed())
			tr.startContainer()

			// The server protects itself only. Rootless, it lacks the
			// privileges for that and keeps the score of the tests.
			server := "-1000"
			if unshare.IsRootless() {
				server = own
			}

			Expect(oomScoreAdj(strconv.FormatUint(uint64(sut.PID()), 10))).To(Equal(server))
			Expect(oomScoreAdj(strconv.FormatUint(uint64(resp.PID), 10))).To(Equal(own))

			result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "cat", "/proc/self/oom_score_adj"},
				Timeout: 10 * time.Second,
			})
			Expect(err).To(Succeed())
			Expect(result.ExitCode).To(BeEquivalentTo(0))
			Expect(strings.TrimSpace(string(result.Stdout))).To(Equal(own))
		}, SpecTimeout(specTimeout))

		for _, terminal := range []bool{true, false} {
			It(testName("should succeed without timeout", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

				result, err := sut.ExecSyncContainer(
					withoutDeadline(ctx),
					&client.ExecSyncContainerConfig{
						ID:       tr.ctrID,
						Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
						Timeout:  timeoutUnlimited,
						Terminal: terminal,
					},
				)

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

				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  10 * time.Second,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())
			}, SpecTimeout(specTimeout))

			It(testName("should set the correct exit code", terminal), func(ctx SpecContext) {
				runContainer(ctx, terminal, []string{"/busybox", "sleep", "10"})

				result, err := sut.ExecSyncContainer(
					withoutDeadline(ctx),
					&client.ExecSyncContainerConfig{
						ID:       tr.ctrID,
						Command:  []string{"/busybox", "invalid"},
						Timeout:  timeoutUnlimited,
						Terminal: terminal,
					},
				)

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
				result, err := sut.ExecSyncContainer(ctx, &client.ExecSyncContainerConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "sleep", "10"},
					Timeout:  3 * time.Second,
					Terminal: terminal,
				})

				Expect(err).To(Succeed())
				// Well before the sleep would end, with slack for a loaded host.
				Expect(time.Since(start)).To(BeNumerically("<", 7*time.Second))
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
					result, err := sut.ExecSyncContainer(execCtx, &client.ExecSyncContainerConfig{
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
							&client.ExecSyncContainerConfig{
								ID:      tr.ctrID,
								Command: []string{"/busybox", "ps"},
								Timeout: 10 * time.Second,
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
})
