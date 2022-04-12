package client_test

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/ioutil"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/containers/storage/pkg/stringid"
	"github.com/containers/storage/pkg/unshare"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-tools/generate"
)

const (
	timeoutUnlimited = 0
)

var (
	busyboxDest = filepath.Join(busyboxDestDir, "busybox")
	runtimePath = os.Getenv("RUNTIME_BINARY")
	conmonPath  = os.Getenv("CONMON_BINARY")
	maxRSSKB    = 230
)

// TestConmonClient runs the created specs
func TestConmonClient(t *testing.T) {
	if rssStr := os.Getenv("MAX_RSS_KB"); rssStr != "" {
		rssInt, err := strconv.Atoi(rssStr)
		if err != nil {
			t.Error(err)
		}
		maxRSSKB = rssInt
	}
	RegisterFailHandler(Fail)
	RunSpecs(t, "ConmonClient")
}

var _ = Describe("ConmonClient", func() {
	var (
		tmpDir, tmpRootfs, ctrID string
		rr                       *RuntimeRunner
	)

	var sut *client.ConmonClient
	createRuntimeConfigWithProcessArgs := func(terminal bool, processArgs []string) {
		tmpDir = MustTempDir("conmon-client")
		rr = &RuntimeRunner{
			runtimeRoot: MustDirInTempDir(tmpDir, "root"),
		}

		// Save busy box binary if we don't have it.
		Expect(cacheBusyBox()).To(BeNil())

		// generate container ID.
		ctrID = stringid.GenerateNonCryptoID()

		// Create Rootfs.
		tmpRootfs = MustDirInTempDir(tmpDir, "rootfs")

		// Link busybox binary to rootfs.
		Expect(os.Link(busyboxDest, filepath.Join(tmpRootfs, "busybox"))).To(BeNil())

		// Finally, create config.json.
		Expect(generateRuntimeConfigWithProcessArgs(tmpDir, tmpRootfs, terminal, processArgs)).To(BeNil())
	}
	createRuntimeConfig := func(terminal bool) {
		createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "ls"})
	}

	containerLogContents := func(path string) string {
		logContents, err := os.ReadFile(path)
		Expect(err).To(BeNil())
		return string(logContents)
	}

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
		Expect(rr.RunCommand("delete", "-f", ctrID)).To(BeNil())
		Expect(os.RemoveAll(tmpDir)).To(BeNil())
		if sut != nil {
			Expect(sut.Shutdown()).To(BeNil())
		}
	})

	Describe("CreateContainer", func() {
		for _, terminal := range []bool{true, false} {
			terminal := terminal
			testName := "should create a simple container"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfig(terminal)

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())
			})

			testName = "should write exit file"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfig(terminal)

				exitPath := MustFileInTempDir(tmpDir, "exit")
				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					ExitPaths:  []string{exitPath},
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				Expect(rr.RunCommand("start", ctrID)).To(BeNil())
				Eventually(func() error {
					f, err := os.Open(exitPath)
					if err != nil {
						return err
					}
					defer f.Close()
					b, err := ioutil.ReadAll(f)
					if err != nil {
						return err
					}
					if string(b) != "0" {
						return errors.New("invalid exit status")
					}
					return nil
				}, time.Second*5).Should(BeNil())
			})

			testName = "should kill created children if being killed"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfig(terminal)

				exitPath := MustFileInTempDir(tmpDir, "exit")
				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					ExitPaths:  []string{exitPath},
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				Expect(sut.Shutdown()).To(BeNil())
				sut = nil

				Eventually(func() error {
					return rr.RunCommandCheckOutput("stopped", "list")
				}, time.Second*5).Should(BeNil())
			})
			It("should return error if invalid command", func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"invalid"})

				exitPath := MustFileInTempDir(tmpDir, "exit")
				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				_, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					ExitPaths:  []string{exitPath},
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).NotTo(BeNil())
			})
			It("should handle long run dir", func() {
				serverRunDir := MustDirInTempDir(tmpDir, "thisisareallylongdirithasmanycharactersinthepathsosuperduperlongannoyinglylong")
				createRuntimeConfig(terminal)

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(serverRunDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())
			})
		}
	})

	Describe("ExecSyncContainer", func() {
		for _, terminal := range []bool{false} {
			terminal := terminal
			testName := "should succeeed without timeout"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"})

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				// Start the container
				Expect(rr.RunCommand("start", ctrID)).To(BeNil())

				// Wait for container to be running
				Eventually(func() error {
					return rr.RunCommandCheckOutput("running", "list")
				}, time.Second*10).Should(BeNil())

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  timeoutUnlimited,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())

				// Log testing
				logs := containerLogContents(logPath)
				Expect(logs).To(ContainSubstring("stdout P hello world\n"))

				sut.ReopenLogContainer(context.Background(), &client.ReopenLogContainerConfig{
					ID: ctrID,
				})
				logs = containerLogContents(logPath)
				Expect(logs).To(BeEmpty())

			})

			testName = "should succeeed with timeout"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"})

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				// Start the container
				Expect(rr.RunCommand("start", ctrID)).To(BeNil())

				// Wait for container to be running
				Eventually(func() error {
					return rr.RunCommandCheckOutput("running", "list")
				}, time.Second*10).Should(BeNil())

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       ctrID,
					Command:  []string{"/busybox", "echo", "-n", "hello", "world"},
					Timeout:  10,
					Terminal: terminal,
				})

				Expect(err).To(BeNil())
				Expect(result.ExitCode).To(BeEquivalentTo(0))
				Expect(result.Stdout).To(BeEquivalentTo("hello world"))
				Expect(result.Stderr).To(BeEmpty())
			})

			testName = "should set the correct exit code"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"})

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				// Start the container
				Expect(rr.RunCommand("start", ctrID)).To(BeNil())

				// Wait for container to be running
				Eventually(func() error {
					return rr.RunCommandCheckOutput("running", "list")
				}, time.Second*10).Should(BeNil())

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       ctrID,
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
				Expect(result.Stdout).To(BeEquivalentTo(expectedStr))
				Expect(result.Stderr).To(BeEmpty())
			})

			testName = "should timeout"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"})

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				// Start the container
				Expect(rr.RunCommand("start", ctrID)).To(BeNil())

				// Wait for container to be running
				Eventually(func() error {
					return rr.RunCommandCheckOutput("running", "list")
				}, time.Second*10).Should(BeNil())

				result, err := sut.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
					ID:       ctrID,
					Command:  []string{"/busybox", "sleep", "10"},
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
			testName := "should succeeed"
			if terminal {
				testName += " with terminal"
			}
			It(testName, func() {
				createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "sleep", "10"})

				logPath := MustFileInTempDir(tmpDir, "log")
				sut = configGivenEnv(tmpDir, rr.runtimeRoot, terminal)
				resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
					ID:         ctrID,
					BundlePath: tmpDir,
					Terminal:   terminal,
					LogDrivers: []client.LogDriver{{
						Type: client.LogDriverTypeContainerRuntimeInterface,
						Path: logPath,
					}},
				})
				Expect(err).To(BeNil())
				Expect(resp.PID).NotTo(Equal(0))
				Eventually(func() error {
					return rr.RunCommandCheckOutput(ctrID, "list")
				}, time.Second*5).Should(BeNil())

				// Start the container
				Expect(rr.RunCommand("start", ctrID)).To(BeNil())

				// Wait for container to be running
				Eventually(func() error {
					return rr.RunCommandCheckOutput("running", "list")
				}, time.Second*10).Should(BeNil())

				// Attach to the container
				socketPath := filepath.Join(tmpDir, "attach")
				err = sut.AttachContainer(context.Background(), &client.AttachConfig{
					ID:         ctrID,
					SocketPath: socketPath,
				})
				Expect(err).To(BeNil())

				err = testAttachSocketConnection(socketPath)
				Expect(err).To(BeNil())
			})
		}
	})
})

func MustTempDir(name string) string {
	d, err := ioutil.TempDir(os.TempDir(), name)
	Expect(err).To(BeNil())
	return d
}

func MustDirInTempDir(parent, name string) string {
	dir := filepath.Join(parent, name)
	Expect(os.MkdirAll(dir, 0755)).To(BeNil())
	return dir
}

func MustFileInTempDir(parent, name string) string {
	file := filepath.Join(parent, name)
	f, err := os.Create(file)
	f.Close()
	Expect(err).To(BeNil())
	return file
}

func configGivenEnv(serverRun, runtimeRoot string, terminal bool) *client.ConmonClient {
	logDriver := client.LogDriverSystemd
	if terminal {
		logDriver = client.LogDriverStdout
	}
	sut, err := client.New(&client.ConmonServerConfig{
		Runtime:          runtimePath,
		ServerRunDir:     serverRun,
		ConmonServerPath: conmonPath,
		Stdout:           os.Stdout,
		Stderr:           os.Stderr,
		RuntimeRoot:      runtimeRoot,
		LogLevel:         "debug",
		LogDriver:        logDriver,
	})
	Expect(err).To(BeNil())
	Expect(sut).NotTo(BeNil())
	return sut
}

func vmRSSGivenPID(pid uint32) uint32 {
	procEntry := filepath.Join("/proc", strconv.Itoa(int(pid)), "status")

	f, err := os.Open(procEntry)
	Expect(err).To(BeNil())
	defer f.Close()

	scanner := bufio.NewScanner(f)

	var rss string
	for scanner.Scan() {
		if !strings.Contains(scanner.Text(), "VmRSS:") {
			continue
		}
		parts := strings.Fields(scanner.Text())
		Expect(len(parts)).To(Equal(3))
		rss = parts[1]
		break
	}
	rssU64, err := strconv.ParseUint(rss, 10, 32)
	Expect(err).To(BeNil())
	return uint32(rssU64)
}

func cacheBusyBox() error {
	if _, err := os.Stat(busyboxDest); err == nil {
		return nil
	}
	if err := os.MkdirAll(busyboxDestDir, 0755); err != nil && !os.IsExist(err) {
		return err
	}
	if err := downloadFile(busyboxSource, busyboxDest); err != nil {
		return err
	}
	if err := os.Chmod(busyboxDest, 0777); err != nil {
		return err
	}
	return nil
}

// source: https://progolang.com/how-to-download-files-in-go/
// downloadFile will download a url and store it in local filepath.
// It writes to the destination file as it downloads it, without
// loading the entire file into memory.
func downloadFile(url string, filepath string) error {
	// Create the file
	out, err := os.Create(filepath)
	if err != nil {
		return err
	}
	defer out.Close()

	// Get the data
	client := http.Client{Timeout: time.Minute}
	resp, err := client.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	// Write the body to file
	_, err = io.Copy(out, resp.Body)
	if err != nil {
		return err
	}

	return nil
}

type RuntimeRunner struct {
	runtimeRoot string
}

func generateRuntimeConfigWithProcessArgs(bundlePath, rootfs string, terminal bool, processArgs []string) error {
	configPath := filepath.Join(bundlePath, "config.json")
	g, err := generate.New("linux")
	if err != nil {
		return err
	}
	g.SetProcessCwd("/")
	g.SetProcessTerminal(terminal)
	g.SetProcessArgs(processArgs)
	g.SetRootPath(rootfs)
	if unshare.IsRootless() {
		specconv.ToRootless(g.Config)
	}

	return g.SaveToFile(configPath, generate.ExportOptions{})
}

func (rr *RuntimeRunner) RunCommand(args ...string) error {
	stdoutString, err := rr.runCommand(args...)
	if err != nil {
		return err
	}
	if stdoutString != "" {
		fmt.Fprintf(GinkgoWriter, stdoutString+"\n")
	}
	return nil
}

func (rr *RuntimeRunner) RunCommandCheckOutput(pattern string, args ...string) error {
	stdoutString, err := rr.runCommand(args...)
	if err != nil {
		return err
	}
	match, _ := regexp.MatchString(pattern, stdoutString)
	if !match {
		return fmt.Errorf("Expected %s to be a substr of %s", pattern, stdoutString)
	}
	return nil
}

func (rr *RuntimeRunner) runCommand(args ...string) (string, error) {
	var stdout bytes.Buffer
	var stderr bytes.Buffer

	cmd := exec.Command(runtimePath, append(rr.runtimeRootArgs(), args...)...)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return "", err
	}
	return stdout.String(), nil
}

func (rr *RuntimeRunner) runtimeRootArgs() []string {
	return []string{"--root", rr.runtimeRoot}
}

func testAttachSocketConnection(socketPath string) error {
	conn, err := client.DialLongSocket("unixpacket", socketPath)
	if err != nil {
		return err
	}
	defer conn.Close()

	outputStream := io.WriteCloser(os.Stdout)
	errorStream := io.WriteCloser(os.Stderr)

	receiveStdout := make(chan error)
	go func() {
		receiveStdout <- redirectResponseToOutputStreams(outputStream, errorStream, conn)
		close(receiveStdout)
	}()
	if err := <-receiveStdout; err != nil {
		return err
	}

	return nil

}

func redirectResponseToOutputStreams(outputStream, errorStream io.WriteCloser, conn io.Reader) (err error) {
	const (
		attachPipeStdout = 2
		attachPipeStderr = 3
		bufSize          = 8192
	)
	reader := bufio.NewReader(conn)
	buf := make([]byte, 0, bufSize)

	for i := 0; i < 2; i++ {
		n, err := io.ReadFull(reader, buf[:cap(buf)])
		buf = buf[:n]
		if err != nil {
			if err == io.EOF {
				break
			}
			if err != io.ErrUnexpectedEOF {
				fmt.Fprintln(os.Stderr, err)
				break
			}
		}

		fmt.Printf("Read %v bytes\n", n)
		if n > 0 {
			var dst io.Writer
			switch buf[0] {
			case attachPipeStdout:
				dst = outputStream
			case attachPipeStderr:
				dst = errorStream
			default:
				return fmt.Errorf("got unexpected attach type %+d", buf[0])
			}
			if dst != nil {
				nw, ew := dst.Write(buf[1:n])
				if ew != nil {
					err = ew
					break
				}
				if n != nw+1 {
					err = io.ErrShortWrite
					break
				}
			}
		}
	}

	return err
}
