package client_test

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/containers/common/pkg/resize"
	"github.com/containers/conmon-rs/pkg/client"
	"github.com/containers/storage/pkg/unshare"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-tools/generate"
)

var _ = Describe("ConmonClient", func() {
	var tr *testRunner
	var sut *client.ConmonClient
	JustAfterEach(func() {
		if sut != nil {
			pid := sut.PID()
			Expect(pid).To(BeNumerically(">", 0))
			rss := vmRSSGivenPID(pid)
			// use Println because GinkgoWriter only writes on failure,
			// and it's interesting to see this value for successful runs too.
			fmt.Println("VmRSS for server is", rss)
			Expect(rss).To(BeNumerically("<", maxRSSKB))
		}
	})

	AfterEach(func() {
		Expect(tr.rr.RunCommand("delete", "-f", tr.ctrID)).To(BeNil())
		if sut != nil {
			Expect(sut.Shutdown()).To(BeNil())
		}
		Expect(os.RemoveAll(tr.tmpDir)).To(BeNil())
	})
	Describe("New", func() {
		It("should restore from running server", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()
			sut2 := tr.configGivenEnv()
			Expect(sut2.PID()).To(Equal(sut.PID()))
		})
	})

	Describe("CreateContainer", func() {
		for _, terminal := range []bool{true, false} {
			terminal := terminal
			It(testName("should create a simple container", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
			})

			It(testName("should write exit file", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)
				Expect(fileContents(tr.exitPath())).To(Equal("0"))
			})

			It(testName("should kill created children if being killed", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)

				Expect(sut.Shutdown()).To(BeNil())
				sut = nil

				Eventually(func() error {
					return tr.rr.RunCommandCheckOutput("stopped", "list")
				}, time.Second*20).Should(BeNil())
			})

			It(testName("should return error if invalid command", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"invalid"}, nil)
				sut = tr.configGivenEnv()
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         tr.ctrID,
					BundlePath: tr.tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: tr.logPath(),
					}},
				})
				Expect(err).NotTo(BeNil())
			})

			It(testName("should handle long run dir", terminal), func() {
				tr = newTestRunner()
				tr.tmpDir = MustDirInTempDir(
					tr.tmpDir,
					"thisisareallylongdirithasmanycharactersinthepathsosuperduperlongannoyinglylong",
				)
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
			})

			It(testName("should succeed/error to set the window size", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)

				err := sut.SetWindowSizeContainer(
					context.Background(),
					&client.SetWindowSizeContainerConfig{
						ID: tr.ctrID,
						Size: &resize.TerminalSize{
							Width:  10,
							Height: 20,
						},
					},
				)
				if terminal {
					Expect(err).To(BeNil())
				} else {
					Expect(err).NotTo(BeNil())
				}
			})

			It(testName("should catch out of memory (oom) events", terminal), func() {
				if unshare.IsRootless() {
					Skip("does not run rootless")
				}

				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "tail", "/dev/zero"},
					func(g generate.Generator) {
						g.SetLinuxResourcesMemoryLimit(1024 * 1024)
					},
				)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				for i := 0; i < 10; i++ {
					if _, err := os.Stat(tr.oomExitPath()); err == nil {
						break
					}
					fmt.Println("Waiting for OOM exit path to exist")
					time.Sleep(time.Second)
				}
				Expect(fileContents(tr.oomExitPath())).To(BeEmpty())
			})

			It(testName("should reopen logs based on max size", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)
				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers[0].MaxSize = 50
				tr.createContainerWithConfig(sut, cfg)
				tr.startContainer(sut)

				logs := fileContents(tr.logPath())
				Expect(logs).NotTo(ContainSubstring("hello"))
			})
		}
	})

	Describe("ExecSync Stress", func() {
		for _, terminal := range []bool{true, false} {
			terminal := terminal
			It(testName("should handle many requests", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "30"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				var wg sync.WaitGroup
				for i := 0; i < 10; i++ {
					wg.Add(1)
					go func(i int) {
						defer GinkgoRecover()
						defer wg.Done()
						result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
							ID:       tr.ctrID,
							Command:  []string{"/busybox", "echo", "-n", "hello", "world", fmt.Sprintf("%d", i)},
							Terminal: terminal,
							Timeout:  timeoutUnlimited,
						})
						Expect(err).To(BeNil())
						Expect(result).NotTo(BeNil())
						Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
						fmt.Println("done with", i, string(result.Stdout))
					}(i)
				}
				wg.Wait()
			})
		}
	})

	Describe("ExecSyncContainer", func() {
		for _, terminal := range []bool{true, false} {
			terminal := terminal
			It(testName("should succeed without timeout", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  timeoutUnlimited,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())

				err = sut.ReopenLogContainer(context.Background(), &client.ReopenLogContainerConfig{
					ID: tr.ctrID,
				})
				Expect(err).To(BeNil())
				logs := fileContents(tr.logPath())
				Expect(logs).To(BeEmpty())
			})

			It(testName("should succeed with timeout", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  10,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())
			})

			It(testName("should set the correct exit code", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "invalid"},
					Timeout:  timeoutUnlimited,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
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
			})

			It(testName("should timeout", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "20"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       tr.ctrID,
					Command:  []string{"/busybox", "sleep", "5"},
					Timeout:  3,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
				Expect(result).NotTo(BeNil())
				Expect(result.TimedOut).To(Equal(true))
			})
		}
	})

	Describe("Attach", func() {
		for _, terminal := range []bool{true, false} {
			terminal := terminal
			It(testName("should succeed", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sh"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				stdin, stdinWrite := io.Pipe()
				stdoutRead, stdout := io.Pipe()
				stderrRead, stderr := io.Pipe()
				// Attach to the container
				socketPath := filepath.Join(tr.tmpDir, "attach")
				go func() {
					defer GinkgoRecover()
					err := sut.AttachContainer(context.Background(), &client.AttachConfig{
						ID:         tr.ctrID,
						SocketPath: socketPath,
						Tty:        terminal,
						Streams: client.AttachStreams{
							Stdin:  &client.In{stdin},
							Stdout: &client.Out{stdout},
							Stderr: &client.Out{stderr},
						},
					})
					Expect(err).To(BeNil())
				}()

				testAttach(stdinWrite, stdoutRead, stderrRead)
			})
		}
	})
})
