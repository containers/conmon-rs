package client_test

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/google/uuid"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runtime-tools/generate"
	"go.podman.io/common/pkg/resize"
	"go.podman.io/storage/pkg/idtools"
	"go.podman.io/storage/pkg/unshare"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/remotecommand"
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
			GinkgoWriter.Println("VmRSS for server is", rss)
			Expect(rss).To(BeNumerically("<", maxRSSKB))
		}
	})

	AfterEach(func() {
		Expect(tr.rr.RunCommand("delete", "-f", tr.ctrID)).To(Succeed())
		if sut != nil {
			Expect(sut.Shutdown()).To(Succeed())
		}
		Expect(os.RemoveAll(tr.tmpDir)).To(Succeed())
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

	Describe("Version", func() {
		for _, verbose := range []bool{false, true} {
			name := "should succeed"
			if verbose {
				name += " with verbose output"
			}
			It(name, func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(false)
				sut = tr.configGivenEnv()

				version, err := sut.Version(
					context.Background(),
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

				if verbose {
					Expect(version.CargoTree).NotTo(BeEmpty())
				} else {
					Expect(version.CargoTree).To(BeEmpty())
				}
			})
		}
	})

	Describe("CreateContainer", func() {
		for _, terminal := range []bool{true, false} {
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

				Expect(sut.Shutdown()).To(Succeed())
				sut = nil

				Eventually(func() error {
					return tr.rr.RunCommandCheckOutput("stopped", "list")
				}, time.Second*20).Should(Succeed())
			})

			It(testName("should execute cleanup command when container exits", terminal), func() {
				tr = newTestRunner()
				filepath := fmt.Sprintf("%s/conmon-client-test%s", os.TempDir(), tr.ctrID)
				tr.createRuntimeConfig(terminal)
				sut = tr.configGivenEnv()
				tr.createContainerWithConfig(sut, &client.CreateContainerConfig{
					ID:           tr.ctrID,
					BundlePath:   tr.tmpDir,
					Terminal:     terminal,
					ExitPaths:    []string{tr.exitPath()},
					OOMExitPaths: []string{tr.oomExitPath()},
					LogDrivers: []client.ContainerLogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: tr.logPath(),
					}},
					CleanupCmd: []string{"touch", filepath},
				})
				tr.startContainer(sut)
				Expect(fileContents(filepath)).To(BeEmpty())
			})

			It(testName("should return error if invalid command", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"invalid"}, nil)
				sut = tr.configGivenEnv()
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         tr.ctrID,
					BundlePath: tr.tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.ContainerLogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: tr.logPath(),
					}},
				})
				Expect(err).NotTo(Succeed())
				Expect(err.Error()).To(ContainSubstring(`executable file not found in $PATH"`))
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
					Expect(err).To(Succeed())
				} else {
					Expect(err).NotTo(Succeed())
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

				for range 10 {
					if _, err := os.Stat(tr.oomExitPath()); err == nil {
						break
					}
					GinkgoWriter.Println("Waiting for OOM exit path to exist")
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
			It(testName("should respect global args", terminal), func() {
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
				tr.createContainerWithConfig(sut, cfg)
				tr.startContainer(sut)

				logs := fileContents(logFile)
				Expect(logs).NotTo(BeEmpty())
			})
		}
	})

	Describe("ExecSync Stress", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should handle many requests", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "30"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				var wg sync.WaitGroup
				for i := range 10 {
					wg.Add(1)
					go func(i int) {
						defer GinkgoRecover()
						defer wg.Done()
						result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
							ID:       tr.ctrID,
							Command:  []string{"/busybox", "echo", "-n", "hello", "world", strconv.Itoa(i)},
							Terminal: terminal,
							Timeout:  timeoutUnlimited,
						})
						Expect(err).To(Succeed())
						Expect(result).NotTo(BeNil())
						Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
						GinkgoWriter.Println("done with", i, string(result.Stdout))
					}(i)
				}
				wg.Wait()
			})

			It(testName("should not leak memory", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "600"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				pid := sut.PID()
				rssBefore := vmRSSGivenPID(pid)
				GinkgoWriter.Printf("VmRSS before: %d\n", rssBefore)

				for i := range 25 {
					result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
						ID:       tr.ctrID,
						Command:  []string{"/busybox", "echo", "-n", "hello", "world", strconv.Itoa(i)},
						Terminal: terminal,
						Timeout:  timeoutUnlimited,
					})

					Expect(err).To(Succeed())
					Expect(result).NotTo(BeNil())
					Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
					GinkgoWriter.Println("done with", i, string(result.Stdout))
				}

				rssAfter := vmRSSGivenPID(pid)
				GinkgoWriter.Printf("VmRSS after: %d\n", rssAfter)
				GinkgoWriter.Printf("VmRSS diff: %d\n", rssAfter-rssBefore)
				Expect(rssAfter - rssBefore).To(BeNumerically("<", 2000))
			})
		}
	})

	Describe("ExecSyncContainer", func() {
		for _, terminal := range []bool{true, false} {
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

				Expect(err).To(Succeed())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())

				err = sut.ReopenLogContainer(context.Background(), &client.ReopenLogContainerConfig{
					ID: tr.ctrID,
				})
				Expect(err).To(Succeed())
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

				Expect(err).To(Succeed())
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

				Expect(err).To(Succeed())
				Expect(result).NotTo(BeNil())
				Expect(result.TimedOut).To(BeTrue())
			})
		}
	})

	Describe("Attach", func() {
		matrix := []struct {
			terminal bool
			pipe     string
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
		}
		for _, test := range matrix {
			terminal := test.terminal
			pipe := test.pipe
			It(testName("should succeed with "+test.pipe, test.terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sh"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				stdinReader, stdinWriter := io.Pipe()
				stdReader, stdWriter := io.Pipe()

				cfg := &client.AttachConfig{
					ID:                tr.ctrID,
					SocketPath:        filepath.Join(tr.tmpDir, "attach"),
					StopAfterStdinEOF: true,
					Streams: client.AttachStreams{
						Stdin: &client.In{stdinReader},
					},
				}
				if pipe == "stdout" {
					cfg.Streams.Stdout = &client.Out{stdWriter}
				} else {
					cfg.Streams.Stderr = &client.Out{stdWriter}
				}

				testAttach(sut, cfg, stdinWriter, stdReader, pipe, pipe == "stderr", terminal)
			})
		}
	})

	Describe("Tracing", func() {
		const contribTracingPath = "../../contrib/tracing/"

		BeforeEach(func() {
			cmd := exec.Command(contribTracingPath + "start")
			fmt.Fprintln(os.Stdout)
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})

		hasTraces := func() bool {
			resp, err := http.Get("http://localhost:16686/api/traces?service=conmonrs")
			Expect(err).NotTo(HaveOccurred())
			defer resp.Body.Close()

			body, err := io.ReadAll(resp.Body)
			Expect(err).NotTo(HaveOccurred())

			type traceData struct {
				Data []interface{} `json:"data"`
			}
			traces := &traceData{}
			Expect(json.Unmarshal(body, traces)).NotTo(HaveOccurred())

			return len(traces.Data) > 0
		}

		It("should succeed", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			tr.enableTracing = true

			sut = tr.configGivenEnv()
			tr.createContainer(sut, false)
			tr.startContainer(sut)

			for range 100 {
				if hasTraces() {
					break
				}

				time.Sleep(time.Second)
			}
		})

		AfterEach(func() {
			cmd := exec.Command(contribTracingPath + "stop")
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})
	})

	Describe("CreateNamespaces", func() {
		It("should succeed without namespaces", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				context.Background(),
				&client.CreateNamespacesConfig{
					PodID: podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())
		})

		It("should fail without pod ID", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				context.Background(),
				&client.CreateNamespacesConfig{},
			)
			Expect(err).NotTo(Succeed())
			Expect(response).To(BeNil())
		})

		It("should succeed without user namespace", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				context.Background(),
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

			Expect(len(response.Namespaces)).To(BeEquivalentTo(5))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUser))
			Expect(response.Namespaces[4].Type).To(Equal(client.NamespaceUTS))

			for i, ns := range response.Namespaces {
				stat, err := os.Lstat(ns.Path)
				Expect(err).To(Succeed())
				Expect(stat.IsDir()).To(BeFalse())
				Expect(stat.Size()).To(BeZero())
				Expect(stat.Mode()).To(Equal(fs.FileMode(0o444)))

				Expect(filepath.Base(ns.Path)).To(Equal(podID))
				Expect(err).To(Succeed())

				const basePath = "/var/run/"
				switch i {
				case 0:
					Expect(ns.Path).To(ContainSubstring(basePath + "ipcns/"))
				case 1:
					Expect(ns.Path).To(ContainSubstring(basePath + "pidns/"))
				case 2:
					Expect(ns.Path).To(ContainSubstring(basePath + "netns/"))
				case 3:
					Expect(ns.Path).To(ContainSubstring(basePath + "userns/"))
				case 4:
					Expect(ns.Path).To(ContainSubstring(basePath + "utsns/"))
				}
			}
		})

		It("should succeed with user namespace and custom base path", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			defer os.RemoveAll(basePath)
			podID := uuid.New().String()

			uids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}
			gids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

			response, err := sut.CreateNamespaces(
				context.Background(),
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

			Expect(len(response.Namespaces)).To(BeEquivalentTo(5))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUser))
			Expect(response.Namespaces[4].Type).To(Equal(client.NamespaceUTS))

			for _, ns := range response.Namespaces {
				stat, err := os.Lstat(ns.Path)
				Expect(err).To(Succeed())
				Expect(stat.IsDir()).To(BeFalse())
				Expect(stat.Size()).To(BeZero())
			}
		})

		It("should fail with user namespace without mappings", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				context.Background(),
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceUser,
					},
				},
			)
			Expect(err).NotTo(Succeed())
			Expect(errors.Is(err, client.ErrMissingIDMappings)).To(BeTrue())
			Expect(response).To(BeNil())
		})
	})
})

var _ = Describe("JSONLogger", func() {
	var tr *testRunner
	var sut *client.ConmonClient

	AfterEach(func() {
		Expect(os.RemoveAll(tr.tmpDir)).To(Succeed())
		if sut != nil {
			Expect(sut.Shutdown()).To(Succeed())
		}
	})

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log in JSON format", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)

				sut = tr.configGivenEnv()
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         tr.ctrID,
					BundlePath: tr.tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.ContainerLogDriver{{
						Type: client.LogDriverTypeJSONLogger,
						Path: tr.logPath(),
					}},
				})
				Expect(err).NotTo(HaveOccurred())
				tr.startContainer(sut)

				logContent, err := os.ReadFile(tr.logPath())
				Expect(err).NotTo(HaveOccurred())
				// Check if the log content is in JSON format
				// This is a basic check assuming that a valid JSON starts and ends with braces '{' and '}'
				Expect(strings.TrimSpace(string(logContent))).To(SatisfyAll(
					Not(BeEmpty()),
					HavePrefix("{"),
					HaveSuffix("}"),
				))
			})
		}
	})
})

var _ = Describe("JournaldLogger", func() {
	var tr *testRunner
	var sut *client.ConmonClient

	AfterEach(func() {
		if sut != nil {
			Expect(sut.Shutdown()).To(Succeed())
		}
	})

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log to journald", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello world && echo foo bar"},
					nil,
				)

				sut = tr.configGivenEnv()
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         tr.ctrID,
					BundlePath: tr.tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.ContainerLogDriver{{Type: client.LogDriverTypeJournald}},
				})
				Expect(err).NotTo(HaveOccurred())
				tr.startContainer(sut)

				// Verify the journal logs
				cmd := exec.Command("journalctl", "-n2", "_COMM=conmonrs")
				stdout := strings.Builder{}
				cmd.Stdout = &stdout
				Expect(cmd.Run()).NotTo(HaveOccurred())
				res := stdout.String()
				Expect(res).To(ContainSubstring("hello world"))
				Expect(res).To(ContainSubstring("foo bar"))
			})
		}
	})
})

var _ = Describe("StreamingServer", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	AfterEach(func() {
		Expect(tr.rr.RunCommand("delete", "-f", tr.ctrID)).To(Succeed())
		if sut != nil {
			Expect(sut.Shutdown()).To(Succeed())
		}
		Expect(os.RemoveAll(tr.tmpDir)).To(Succeed())
	})

	Describe("ServeExecContainer", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should succeed", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
				sut = tr.configGivenEnv()
				tr.createContainer(sut, terminal)
				tr.startContainer(sut)

				result, err := sut.ServeExecContainer(context.Background(), &client.ServeExecContainerConfig{
					ID:      tr.ctrID,
					Command: []string{"/busybox", "sh", "-c", "echo -n stdout && echo -n stderr >&2"},
					Tty:     terminal,
					Stdout:  true,
					Stderr:  true,
				})

				Expect(err).To(Succeed())
				Expect(result.URL).NotTo(BeEmpty())

				config := &rest.Config{TLSClientConfig: rest.TLSClientConfig{Insecure: true}}
				executor, err := remotecommand.NewWebSocketExecutor(config, "GET", result.URL)
				Expect(err).NotTo(HaveOccurred())

				stdout := &bytes.Buffer{}
				stderr := &bytes.Buffer{}

				streamOptions := remotecommand.StreamOptions{
					Stdout: stdout,
					Stderr: stderr,
					Tty:    terminal,
				}
				err = executor.StreamWithContext(context.Background(), streamOptions)
				Expect(err).NotTo(HaveOccurred())

				if terminal {
					Expect(stdout.String()).To(Equal("stdoutstderr"))
					Expect(stderr.Len()).To(BeZero())
				} else {
					Expect(stdout.String()).To(Equal("stdout"))
					Expect(stderr.String()).To(Equal("stderr"))
				}
			})

			It(testName("should fail if container does not exist", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
				sut = tr.configGivenEnv()

				result, err := sut.ServeExecContainer(context.Background(), &client.ServeExecContainerConfig{
					ID:      "wrong",
					Command: []string{"/busybox", "sh", "-c", "echo -n stdout && echo -n stderr >&2"},
					Tty:     terminal,
					Stdout:  true,
					Stderr:  true,
				})

				Expect(err).To(HaveOccurred())
				Expect(result).To(BeNil())
			})
		}
	})

	Describe("ServeAttachContainer", func() {
		const terminal = true

		It(testName("should succeed", terminal), func() {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "watch", "-n0.5", "echo", "test"}, nil)
			sut = tr.configGivenEnv()
			tr.createContainer(sut, terminal)
			tr.startContainer(sut)

			result, err := sut.ServeAttachContainer(context.Background(), &client.ServeAttachContainerConfig{
				ID:     tr.ctrID,
				Stdin:  true,
				Stdout: true,
				Stderr: true,
			})

			Expect(err).To(Succeed())
			Expect(result.URL).NotTo(BeEmpty())

			config := &rest.Config{TLSClientConfig: rest.TLSClientConfig{Insecure: true}}
			executor, err := remotecommand.NewWebSocketExecutor(config, "GET", result.URL)
			Expect(err).NotTo(HaveOccurred())

			stdout := &bytes.Buffer{}
			stderr := &bytes.Buffer{}

			streamOptions := remotecommand.StreamOptions{
				Stdout: stdout,
				Stderr: stderr,
				Tty:    terminal,
			}

			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()

			err = executor.StreamWithContext(ctx, streamOptions)
			Expect(err).To(HaveOccurred())
			Expect(tr.rr.RunCommand("delete", "-f", tr.ctrID)).To(Succeed())
			Expect(stdout.String()).To(ContainSubstring("echo test"))
			Expect(stderr.String()).To(BeEmpty())
		})

		It(testName("should fail if container does not exist", terminal), func() {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"}, nil)
			sut = tr.configGivenEnv()

			result, err := sut.ServeAttachContainer(context.Background(), &client.ServeAttachContainerConfig{
				ID: "wrong",
			})

			Expect(err).To(HaveOccurred())
			Expect(result).To(BeNil())
		})
	})
})
