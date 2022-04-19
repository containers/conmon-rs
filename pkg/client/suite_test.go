package client_test

import (
	"bufio"
	"bytes"
	"context"
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

type testRunner struct {
	tmpDir, tmpRootfs, ctrID string
	rr                       *RuntimeRunner
}

func newTestRunner() *testRunner {
	return &testRunner{
		tmpDir: MustTempDir("conmon-client"),
	}
}

func (tr *testRunner) createRuntimeConfig(terminal bool) {
	tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "ls"})
}

func (tr *testRunner) createRuntimeConfigWithProcessArgs(terminal bool, processArgs []string) {
	rr := &RuntimeRunner{
		runtimeRoot: MustDirInTempDir(tr.tmpDir, "root"),
	}

	// Save busy box binary if we don't have it.
	Expect(cacheBusyBox()).To(BeNil())

	// generate container ID.
	ctrID := stringid.GenerateNonCryptoID()

	// Create Rootfs.
	tmpRootfs := MustDirInTempDir(tr.tmpDir, "rootfs")

	// Link busybox binary to rootfs.
	Expect(os.Link(busyboxDest, filepath.Join(tmpRootfs, "busybox"))).To(BeNil())

	// Finally, create config.json.
	Expect(generateRuntimeConfigWithProcessArgs(tr.tmpDir, tmpRootfs, terminal, processArgs)).To(BeNil())
	tr.rr = rr
	tr.ctrID = ctrID
	tr.tmpRootfs = tmpRootfs
	MustFile(tr.logPath())
}

func (tr *testRunner) logPath() string {
	return filepath.Join(tr.tmpDir, "log")
}

func (tr *testRunner) exitPath() string {
	return filepath.Join(tr.tmpDir, "exit")
}

func fileContents(path string) string {
	contents, err := os.ReadFile(path)
	Expect(err).To(BeNil())
	return string(contents)
}

func (tr *testRunner) createContainer(sut *client.ConmonClient, terminal bool) {
	resp, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
		ID:         tr.ctrID,
		BundlePath: tr.tmpDir,
		Terminal:   terminal,
		ExitPaths:  []string{tr.exitPath()},
		LogDrivers: []client.LogDriver{{
			Type: client.LogDriverTypeContainerRuntimeInterface,
			Path: tr.logPath(),
		}},
	})
	Expect(err).To(BeNil())
	Expect(resp.PID).NotTo(Equal(0))
	Eventually(func() error {
		return tr.rr.RunCommandCheckOutput(tr.ctrID, "list")
	}, time.Second*5).Should(BeNil())
}

func (tr *testRunner) startContainer(sut *client.ConmonClient) {
	// Start the container
	Expect(tr.rr.RunCommand("start", tr.ctrID)).To(BeNil())

	// Wait for container to be running
	Eventually(func() error {
		if err := tr.rr.RunCommandCheckOutput("running", "list"); err == nil {
			return nil
		}
		return tr.rr.RunCommandCheckOutput("stopped", "list")
	}, time.Second*10).Should(BeNil())
}

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

func MustFile(file string) string {
	f, err := os.Create(file)
	f.Close()
	Expect(err).To(BeNil())
	return file
}

func (tr *testRunner) configGivenEnv() *client.ConmonClient {
	sut, err := client.New(&client.ConmonServerConfig{
		ServerRunDir:     tr.tmpDir,
		RuntimeRoot:      tr.rr.runtimeRoot,
		Runtime:          runtimePath,
		ConmonServerPath: conmonPath,
		Stdout:           os.Stdout,
		Stderr:           os.Stderr,
		LogLevel:         "debug",
		LogDriver:        client.LogDriverStdout,
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

func testName(testName string, terminal bool) string {
	if terminal {
		testName += " with terminal"
	}
	return testName
}

func testAttachSocketConnection(socketPath string) {
	conn, err := client.DialLongSocket("unixpacket", socketPath)
	Expect(err).To(BeNil())
	defer conn.Close()

	// This second connection should be cleaned-up automatically
	go func() {
		conn, err := client.DialLongSocket("unixpacket", socketPath)
		if err != nil {
			panic(err)
		}
		conn.Close()
	}()

	// Stdin
	_, err = conn.Write([]byte("Hello world"))
	Expect(err).To(BeNil())

	const (
		attachPipeStdout = 2
		bufSize          = 8192
	)

	reader := bufio.NewReader(conn)
	buf := make([]byte, 0, bufSize)

	// Stdout test
	n, err := io.ReadFull(reader, buf[:cap(buf)])
	Expect(err).To(BeNil())
	Expect(n).To(Equal(bufSize))
	buf = buf[:n]

	Expect(buf[0]).To(BeEquivalentTo(attachPipeStdout))

	res := string(buf[1:bytes.IndexByte(buf, 0)])
	Expect(res).To(Equal("Hello world"))
}
