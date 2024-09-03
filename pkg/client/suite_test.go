package client_test

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime/pprof"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/containers/storage/pkg/stringid"
	"github.com/containers/storage/pkg/unshare"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-tools/generate"
	"github.com/sirupsen/logrus"
)

const (
	timeoutUnlimited = 0
	conmonBinaryKey  = "CONMON_BINARY"
)

var (
	busyboxDest = filepath.Join(busyboxDestDir, "busybox")
	runtimePath = os.Getenv("RUNTIME_BINARY")
	conmonPath  = os.Getenv(conmonBinaryKey)
	maxRSSKB    = 9500
)

// TestConmonClient runs the created specs.
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

var _ = AfterSuite(func() {
	By("printing the goroutine stack for debugging purposes")
	goroutines := pprof.Lookup("goroutine")
	Expect(goroutines.WriteTo(os.Stdout, 1)).To(Succeed())

	By("Verifying that no conmonrs processes are still running in the background")
	cmd := exec.Command("ps", "aux")
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	Expect(cmd.Run()).To(Succeed())
	scanner := bufio.NewScanner(strings.NewReader(stdout.String()))
	for scanner.Scan() {
		text := scanner.Text()
		if strings.Contains(text, conmonBinaryKey) {
			continue
		}
		Expect(text).NotTo(ContainSubstring(conmonPath))
	}
})

type testRunner struct {
	tmpDir, tmpRootfs, ctrID string
	enableTracing            bool
	rr                       *RuntimeRunner
}

func newTestRunner() *testRunner {
	return &testRunner{
		tmpDir: MustTempDir("conmon-client"),
	}
}

func (tr *testRunner) createRuntimeConfig(terminal bool) {
	tr.createRuntimeConfigWithProcessArgs(terminal, []string{"/busybox", "ls"}, nil)
}

func (tr *testRunner) createRuntimeConfigWithProcessArgs(
	terminal bool, processArgs []string, changeSpec func(generate.Generator),
) {
	rr := &RuntimeRunner{
		runtimeRoot: MustDirInTempDir(tr.tmpDir, "root"),
	}

	// Save busy box binary if we don't have it.
	Expect(cacheBusyBox()).To(Succeed())

	// generate container ID.
	ctrID := stringid.GenerateNonCryptoID()

	// Create Rootfs.
	tmpRootfs := MustDirInTempDir(tr.tmpDir, "rootfs")

	// Link busybox binary to rootfs.
	Expect(os.Link(busyboxDest, filepath.Join(tmpRootfs, "busybox"))).To(Succeed())

	// Finally, create config.json.
	Expect(generateRuntimeConfigWithProcessArgs(
		tr.tmpDir, tmpRootfs, terminal, processArgs, changeSpec,
	)).To(Succeed())
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

func (tr *testRunner) oomExitPath() string {
	return filepath.Join(tr.tmpDir, "oom_exit")
}

func fileContents(path string) string {
	contents, err := os.ReadFile(path)
	Expect(err).To(Succeed())

	return string(contents)
}

func (tr *testRunner) defaultConfig(terminal bool) *client.CreateContainerConfig {
	return &client.CreateContainerConfig{
		ID:           tr.ctrID,
		BundlePath:   tr.tmpDir,
		Terminal:     terminal,
		Stdin:        true,
		ExitPaths:    []string{tr.exitPath()},
		OOMExitPaths: []string{tr.oomExitPath()},
		LogDrivers: []client.ContainerLogDriver{{
			Type: client.LogDriverTypeContainerRuntimeInterface,
			Path: tr.logPath(),
		}},
		CleanupCmd:  []string{},
		GlobalArgs:  []string{},
		CommandArgs: []string{},
	}
}

func (tr *testRunner) createContainer(sut *client.ConmonClient, terminal bool) {
	tr.createContainerWithConfig(sut, tr.defaultConfig(terminal))
}

func (tr *testRunner) createContainerWithConfig(sut *client.ConmonClient, cfg *client.CreateContainerConfig) {
	resp, err := sut.CreateContainer(context.Background(), cfg)
	Expect(err).To(Succeed())
	Expect(resp.PID).NotTo(BeEquivalentTo(0))
	Eventually(func() error {
		return tr.rr.RunCommandCheckOutput(tr.ctrID, "list")
	}, time.Second*5).Should(Succeed())
}

func (tr *testRunner) startContainer(*client.ConmonClient) {
	// Start the container
	Expect(tr.rr.RunCommand("start", tr.ctrID)).To(Succeed())

	// Wait for container to be running
	Eventually(func() error {
		if err := tr.rr.RunCommandCheckOutput("running", "list"); err == nil {
			return nil
		}

		return tr.rr.RunCommandCheckOutput("stopped", "list")
	}, time.Second*10).Should(Succeed())
}

func MustTempDir(name string) string {
	d, err := os.MkdirTemp(os.TempDir(), name)
	Expect(err).To(Succeed())

	return d
}

func MustDirInTempDir(parent, name string) string {
	dir := filepath.Join(parent, name)
	Expect(os.MkdirAll(dir, 0o755)).To(Succeed())

	return dir
}

func MustFile(file string) string {
	f, err := os.Create(file)
	f.Close()
	Expect(err).To(Succeed())

	return file
}

func (tr *testRunner) configGivenEnv() *client.ConmonClient {
	cfg := client.NewConmonServerConfig(runtimePath, tr.rr.runtimeRoot, tr.tmpDir)
	cfg.ConmonServerPath = conmonPath
	cfg.LogDriver = client.LogDriverStdout

	logger := logrus.StandardLogger()
	logger.Level = logrus.TraceLevel
	cfg.ClientLogger = logger

	if tr.enableTracing {
		cfg.Tracing = &client.Tracing{Enabled: true}
	}

	sut, err := client.New(cfg)
	Expect(err).To(Succeed())
	Expect(sut).NotTo(BeNil())

	return sut
}

func vmRSSGivenPID(pid uint32) uint32 {
	const procPath = "/proc"
	procEntry := filepath.Join(procPath, strconv.Itoa(int(pid)), "status")

	f, err := os.Open(procEntry)
	Expect(err).To(Succeed())
	defer f.Close()

	scanner := bufio.NewScanner(f)

	var rss string
	for scanner.Scan() {
		if !strings.Contains(scanner.Text(), "VmRSS:") {
			continue
		}
		parts := strings.Fields(scanner.Text())
		Expect(parts).To(HaveLen(3))
		rss = parts[1]

		break
	}
	rssU64, err := strconv.ParseUint(rss, 10, 32)
	Expect(err).To(Succeed())

	return uint32(rssU64)
}

func cacheBusyBox() error {
	if _, err := os.Stat(busyboxDest); err == nil {
		return nil
	}
	if err := os.MkdirAll(busyboxDestDir, 0o755); err != nil && !os.IsExist(err) {
		return fmt.Errorf("create busybox dest dir: %w", err)
	}
	if err := downloadFile(busyboxSource, busyboxDest); err != nil {
		return fmt.Errorf("download busybox: %w", err)
	}
	if err := os.Chmod(busyboxDest, 0o777); err != nil {
		return fmt.Errorf("change busybox permissions: %w", err)
	}

	return nil
}

// source: https://progolang.com/how-to-download-files-in-go/
// downloadFile will download a url and store it in local path.
// It writes to the destination file as it downloads it, without
// loading the entire file into memory.
func downloadFile(url, path string) error {
	// Create the file
	out, err := os.Create(path)
	if err != nil {
		return fmt.Errorf("create path: %w", err)
	}
	defer out.Close()

	// Get the data
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	c := http.Client{Timeout: time.Minute}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, http.NoBody)
	if err != nil {
		return fmt.Errorf("create request: %w", err)
	}
	resp, err := c.Do(req)
	if err != nil {
		return fmt.Errorf("get URL: %w", err)
	}
	defer resp.Body.Close()

	// Write the body to file
	_, err = io.Copy(out, resp.Body)
	if err != nil {
		return fmt.Errorf("copy response: %w", err)
	}

	return nil
}

type RuntimeRunner struct {
	runtimeRoot string
}

func generateRuntimeConfigWithProcessArgs(
	bundlePath,
	rootfs string,
	terminal bool,
	processArgs []string,
	changeSpec func(generate.Generator),
) error {
	configPath := filepath.Join(bundlePath, "config.json")
	g, err := generate.New("linux")
	if err != nil {
		return fmt.Errorf("create linux config: %w", err)
	}
	g.SetProcessCwd("/")
	g.SetProcessTerminal(terminal)
	g.SetProcessArgs(processArgs)
	g.SetRootPath(rootfs)
	if changeSpec != nil {
		changeSpec(g)
	}
	if unshare.IsRootless() {
		specconv.ToRootless(g.Config)
	}

	if err := g.SaveToFile(configPath, generate.ExportOptions{}); err != nil {
		return fmt.Errorf("save to file: %w", err)
	}

	return nil
}

func (rr *RuntimeRunner) RunCommand(args ...string) error {
	stdoutString, err := rr.runCommand(args...)
	if err != nil {
		return err
	}
	if stdoutString != "" {
		fmt.Fprintf(GinkgoWriter, "%s\n", stdoutString)
	}

	return nil
}

var errNoMatch = errors.New("regex does not match")

func (rr *RuntimeRunner) RunCommandCheckOutput(pattern string, args ...string) error {
	stdoutString, err := rr.runCommand(args...)
	if err != nil {
		return err
	}
	match, err := regexp.MatchString(pattern, stdoutString)
	if err != nil {
		return fmt.Errorf("match regex pattern: %w", err)
	}
	if !match {
		return fmt.Errorf("expected %s to be a substr of %s: %w", pattern, stdoutString, errNoMatch)
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
		return "", fmt.Errorf("run command: %w", err)
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

func testAttach(
	sut *client.ConmonClient,
	cfg *client.AttachConfig,
	stdinWriter io.Writer,
	reader *io.PipeReader,
	testString string,
	useStdErr bool,
	terminal bool,
) {
	wg := sync.WaitGroup{}
	wg.Add(2)

	command := "/busybox echo -n " + testString
	go func() {
		defer wg.Done()
		defer GinkgoRecover()
		pipe := ""
		if useStdErr {
			pipe = " >&2"
		}
		// Print in synchrony to prevent races with terminals.
		// Run twice to ensure all data is processed.
		for range 2 {
			_, err := fmt.Fprintf(stdinWriter, "%s%s\n", command, pipe)
			Expect(err).To(Succeed())
			verifyBuffer(reader, terminal, command, testString)
		}

		// terminate the container
		_, err := fmt.Fprintf(stdinWriter, "exit\n")
		Expect(err).To(Succeed())

		Expect(reader.Close()).To(Succeed())
	}()

	go func() {
		defer wg.Done()
		defer GinkgoRecover()
		err := sut.AttachContainer(context.Background(), cfg)
		// The test races with itself, and sometimes is EOF and sometimes passes
		if !errors.Is(err, io.ErrClosedPipe) {
			Expect(err).To(Succeed())
		}
	}()

	wg.Wait()
}

func verifyBuffer(reader io.Reader, terminal bool, command, expected string) {
	readSection := func() string {
		data := make([]byte, 8191)
		_, err := reader.Read(data)
		Expect(err).To(Succeed())

		return string(bytes.Trim(data, "\x00"))
	}
	if !terminal {
		Expect(readSection()).To(Equal(expected))

		return
	}

	fullExpectedBuffer := command + "\r\n" + expected + "/ # \x1b[6n"
	str := ""
	for {
		str += readSection()
		if len(str) < len(fullExpectedBuffer) {
			continue
		}
		Expect(str).To(Equal(fullExpectedBuffer))

		return
	}
}
