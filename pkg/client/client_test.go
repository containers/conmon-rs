package client_test

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
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
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

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

				// Shutdown should kill the container and clean up
				Expect(sut.Shutdown()).To(Succeed())
				sut = nil

				// Verify the container process is no longer running by checking container state
				// The container may be in "stopped" state or removed entirely depending on runtime
				Eventually(func() bool {
					// If we can't find the container in "created" state, it's been handled
					err := tr.rr.RunCommandCheckOutput("created", "list")

					return err != nil
				}, time.Second*5).Should(BeTrue())
			})

			It(testName("should execute cleanup command when container exits", terminal), func() {
				tr = newTestRunner()
				tr.createRuntimeConfig(terminal)
				// The container ID is only known after the runtime config exists.
				cleanupPath := filepath.Join(tr.tmpDir, "cleanup-"+tr.ctrID)
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
					CleanupCmd: []string{"touch", cleanupPath},
				})
				tr.startContainer(sut)
				Eventually(cleanupPath, 10*time.Second).Should(BeAnExistingFile())
				Expect(fileContents(cleanupPath)).To(BeEmpty())
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
				Expect(err.Error()).To(ContainSubstring(`not found in $PATH`))
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

				Eventually(tr.oomExitPath, 10*time.Second).Should(BeAnExistingFile())
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

			It(testName(name, test.terminal), func() {
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
					ContainerStdin:    true,
					StopAfterStdinEOF: !leaveStdinOpen,
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

		// attachAll attaches stdin, stdout and stderr to a container running the
		// script, writes the input and closes stdin if requested. The
		// beforeAttach function runs after starting the container.
		attachAll := func(
			script string, terminal, stopAfterStdinEOF bool, input string, closeStdin bool,
			changeSpec func(generate.Generator), beforeAttach func(),
		) (attachErr <-chan error, stdout, stderr <-chan string) {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sh", "-c", script}, changeSpec)
			sut = tr.configGivenEnv()
			tr.createContainer(sut, terminal)
			tr.startContainer(sut)

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
					Stdin:  &client.In{stdinReader},
					Stdout: &client.Out{stdoutWriter},
					Stderr: &client.Out{stderrWriter},
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
				errCh <- sut.AttachContainer(context.Background(), cfg)
			}()

			return errCh, readAll(stdoutReader), readAll(stderrReader)
		}

		It("should receive the output written after stdin EOF", func() {
			attachErr, stdout, stderr := attachAll(
				"/busybox cat && echo stdin closed", false, true, "attached", true, nil, nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(Equal("attachedstdin closed\n")))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		})

		It("should receive all output if stdin stays open", func() {
			attachErr, stdout, stderr := attachAll(
				"read -r; echo one; /busybox sleep 0.5; echo two", false, false, "go\n", false, nil, nil,
			)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(Equal("one\ntwo\n")))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		})

		It("should return on stdin EOF if stdin stays open", func() {
			attachErr, stdout, stderr := attachAll("/busybox sleep 1000", false, false, "", true, nil, nil)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
			Eventually(stdout, 10*time.Second).Should(Receive(BeEmpty()))
			Eventually(stderr, 10*time.Second).Should(Receive(BeEmpty()))
		})

		It("should return on stdin EOF with terminal and stdin once", func() {
			attachErr, _, _ := attachAll("/busybox sleep 1000", true, true, "", true, nil, nil)

			Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
		})

		It("should receive the done packet if the output ended before attaching", func() {
			attachErr, stdout, stderr := attachAll(
				"exec >&- 2>&-; /busybox touch /output-closed; /busybox cat", false, true, "", true, nil,
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
		})

		It("should catch out of memory (oom) events after stdin EOF", func() {
			if unshare.IsRootless() {
				Skip("does not run rootless")
			}

			attachErr, _, _ := attachAll(
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
		})
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

		It("should succeed", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			tr.enableTracing = true

			sut = tr.configGivenEnv()

			// Tracing is a compile time feature of the server, only a build
			// with it can export spans.
			version, err := sut.Version(context.Background(), &client.VersionConfig{Verbose: true})
			Expect(err).NotTo(HaveOccurred())

			if !strings.Contains(version.CargoTree, "opentelemetry") {
				Skip("server built without the tracing feature")
			}

			tr.createContainer(sut, false)
			tr.startContainer(sut)

			Eventually(hasTraces, 100*time.Second, time.Second).Should(BeTrue())
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
		})

		It("should not keep a pause process without PID namespace", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				context.Background(),
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
		})

		It("should not keep a pause process with user namespace and without PID namespace", func() {
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
				context.Background(),
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
		})

		It("should succeed with user namespace and custom base path", func() {
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
		})

		It("should reap orphans in the pod PID namespace", func() {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				context.Background(),
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
				context.Background(),
				"nsenter", "--pid="+response.Namespaces[0].Path, "--",
				"sh", "-c", "sleep 1 & exit 0",
			)
			Expect(cmd.Run()).To(Succeed())
			Expect(childrenOf(pausePID)).NotTo(BeEmpty())

			// Once the orphan exited, it does not stay a zombie.
			Eventually(func() []int { return childrenOf(pausePID) }).
				WithTimeout(10 * time.Second).
				Should(BeEmpty())
		})

		It("should fail for another pod or configuration", func() {
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
			response, err := sut.CreateNamespaces(context.Background(), &cfg)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(2))

			// The same request is answered with the same namespaces, in any order.
			same := cfg
			same.Namespaces = []client.Namespace{client.NamespaceUTS, client.NamespaceIPC}
			again, err := sut.CreateNamespaces(context.Background(), &same)
			Expect(err).To(Succeed())
			Expect(again.Namespaces).To(Equal(response.Namespaces))

			otherPod := cfg
			otherPod.PodID = uuid.New().String()
			_, err = sut.CreateNamespaces(context.Background(), &otherPod)
			Expect(err).NotTo(Succeed())

			otherNamespaces := cfg
			otherNamespaces.Namespaces = []client.Namespace{client.NamespaceNet}
			_, err = sut.CreateNamespaces(context.Background(), &otherNamespaces)
			Expect(err).NotTo(Succeed())

			otherBasePath := cfg
			otherBasePath.BasePath = MustTempDir("ns-test-")
			DeferCleanup(os.RemoveAll, otherBasePath.BasePath)
			_, err = sut.CreateNamespaces(context.Background(), &otherBasePath)
			Expect(err).NotTo(Succeed())
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
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

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
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	AfterEach(func() {
		if sut != nil {
			Expect(sut.Shutdown()).To(Succeed())
		}
	})

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log to journald", terminal), func() {
				tr = newTestRunner()
				// Unique markers, other specs may log to the journal at the same time.
				marker := uuid.New().String()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello " + marker + " && echo foo " + marker},
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

				// Verify the journal logs, they arrive asynchronously.
				Eventually(func(g Gomega) string {
					cmd := exec.Command("journalctl", "--since", "-1min", "_COMM=conmonrs")
					stdout := strings.Builder{}
					cmd.Stdout = &stdout
					g.Expect(cmd.Run()).NotTo(HaveOccurred())

					return stdout.String()
				}, 10*time.Second).Should(And(
					ContainSubstring("hello "+marker),
					ContainSubstring("foo "+marker),
				))
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

				config := &rest.Config{Insecure: true}
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

		It("should receive the output written after stdin EOF", func() {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(false, []string{"/busybox", "sleep", "10"}, nil)
			sut = tr.configGivenEnv()
			tr.createContainer(sut, false)
			tr.startContainer(sut)

			result, err := sut.ServeExecContainer(context.Background(), &client.ServeExecContainerConfig{
				ID:      tr.ctrID,
				Command: []string{"/busybox", "sh", "-c", "/busybox cat && echo stdin closed"},
				Stdin:   true,
				Stdout:  true,
				Stderr:  true,
			})
			Expect(err).To(Succeed())

			executor, err := remotecommand.NewWebSocketExecutor(&rest.Config{Insecure: true}, "GET", result.URL)
			Expect(err).NotTo(HaveOccurred())

			stdout := &bytes.Buffer{}
			stderr := &bytes.Buffer{}

			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			Expect(executor.StreamWithContext(ctx, remotecommand.StreamOptions{
				Stdin:  strings.NewReader("attached"),
				Stdout: stdout,
				Stderr: stderr,
			})).To(Succeed())
			Expect(stdout.String()).To(Equal("attachedstdin closed\n"))
			Expect(stderr.String()).To(BeEmpty())
		})
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

			config := &rest.Config{Insecure: true}
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

		It("should receive the output written after stdin EOF with stdin once", func() {
			tr = newTestRunner()
			tr.createRuntimeConfigWithProcessArgs(
				false, []string{"/busybox", "sh", "-c", "/busybox cat && echo stdin closed"}, nil,
			)
			sut = tr.configGivenEnv()
			tr.createContainer(sut, false)
			tr.startContainer(sut)

			result, err := sut.ServeAttachContainer(context.Background(), &client.ServeAttachContainerConfig{
				ID:                tr.ctrID,
				Stdin:             true,
				Stdout:            true,
				Stderr:            true,
				StopAfterStdinEOF: true,
			})
			Expect(err).To(Succeed())

			executor, err := remotecommand.NewWebSocketExecutor(&rest.Config{Insecure: true}, "GET", result.URL)
			Expect(err).NotTo(HaveOccurred())

			stdout := &bytes.Buffer{}
			stderr := &bytes.Buffer{}

			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()

			Expect(executor.StreamWithContext(ctx, remotecommand.StreamOptions{
				Stdin:  strings.NewReader("attached"),
				Stdout: stdout,
				Stderr: stderr,
			})).To(Succeed())
			Expect(stdout.String()).To(Equal("attachedstdin closed\n"))
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
