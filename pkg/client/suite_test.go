package client_test

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
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
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-tools/generate"
	"github.com/sirupsen/logrus"
	"go.podman.io/storage/pkg/stringid"
	"go.podman.io/storage/pkg/unshare"
	"golang.org/x/sys/unix"
)

const (
	timeoutUnlimited = 0
	conmonBinaryKey  = "CONMON_BINARY"
)

var (
	errBusyboxMissing   = errors.New("BUSYBOX_BINARY does not exist")
	errDownloadStatus   = errors.New("unexpected download status")
	errChecksumMismatch = errors.New("checksum mismatch")
)

var (
	busyboxDest = filepath.Join(busyboxDestDir, "busybox")
	runtimePath = os.Getenv("RUNTIME_BINARY")
	conmonPath  = os.Getenv(conmonBinaryKey)
	// The budget of the Makefile for a dynamically linked release build,
	// MAX_RSS_KB overrides it. See the Makefile for the budgets.
	maxRSSKB = 10240
)

// TestConmonClient runs the created specs.
func TestConmonClient(t *testing.T) {
	if rssStr := os.Getenv("MAX_RSS_KB"); rssStr != "" {
		rssInt, err := strconv.Atoi(rssStr)
		if err != nil {
			t.Fatalf("parse MAX_RSS_KB: %v", err)
		}

		maxRSSKB = rssInt
	}

	if binary := os.Getenv("BUSYBOX_BINARY"); binary != "" {
		busyboxDest = binary
	}

	RegisterFailHandler(Fail)
	RunSpecs(t, "ConmonClient")
}

var _ = AfterSuite(func() {
	By("printing the goroutine stack for debugging purposes")

	goroutines := pprof.Lookup("goroutine")
	Expect(goroutines.WriteTo(os.Stdout, 1)).To(Succeed())

	By("Verifying that no conmonrs processes are still running in the background")

	cmd := exec.CommandContext(context.Background(), "ps", "aux")

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

	// Link busybox binary to rootfs, or copy it if it lives on another
	// file system or belongs to another user.
	Expect(linkOrCopy(busyboxDest, filepath.Join(tmpRootfs, "busybox"))).To(Succeed())

	// Finally, create config.json.
	Expect(generateRuntimeConfigWithProcessArgs(
		tr.tmpDir, tmpRootfs, terminal, processArgs, changeSpec,
	)).To(Succeed())

	tr.rr = rr
	tr.ctrID = ctrID
	tr.tmpRootfs = tmpRootfs
	MustFile(tr.logPath())
}

// expectNamespaceFile verifies that a namespace bind mount exists at the path:
// a plain file that is backed by the nsfs filesystem rather than the empty
// regular file the mount target was created from. The namespace has to be a
// new one, not the one of the test process.
func expectNamespaceFile(path string) {
	stat, err := os.Lstat(path)
	Expect(err).To(Succeed())
	Expect(stat.IsDir()).To(BeFalse())
	Expect(stat.Size()).To(BeZero())

	var statfs unix.Statfs_t

	Expect(unix.Statfs(path, &statfs)).To(Succeed())
	Expect(statfs.Type).To(BeEquivalentTo(unix.NSFS_MAGIC), "%s is not a namespace", path)

	// The path is <base>/<type>ns/<pod ID>.
	nsType := strings.TrimSuffix(filepath.Base(filepath.Dir(path)), "ns")

	var bound, own unix.Stat_t

	Expect(unix.Stat(path, &bound)).To(Succeed())
	Expect(unix.Stat("/proc/self/ns/"+nsType, &own)).To(Succeed())
	Expect(bound.Ino).NotTo(Equal(own.Ino), "%s is the %s namespace of the host", path, nsType)
}

// pauseProcessRunning returns true if a conmonrs pause process for the pod is
// still alive.
func pauseProcessRunning(podID string) bool {
	cmd := exec.CommandContext(context.Background(), "ps", "-eo", "args")

	var stdout bytes.Buffer

	cmd.Stdout = &stdout
	Expect(cmd.Run()).To(Succeed())

	for line := range strings.SplitSeq(stdout.String(), "\n") {
		if strings.Contains(line, " pause ") && strings.Contains(line, podID) {
			return true
		}
	}

	return false
}

// execPIDFiles returns the PID files of execs left in the runtime directory.
func execPIDFiles(runtimeDir string) []string {
	files, err := filepath.Glob(filepath.Join(runtimeDir, "exec_*pid"))
	Expect(err).To(Succeed())

	return files
}

// childrenOf returns the PIDs of all processes whose parent is pid, zombies
// included.
func childrenOf(pid int) []int {
	return childrenOfWithState(pid, "")
}

// zombieChildrenOf returns the PIDs of the zombie children of pid.
func zombieChildrenOf(pid int) []int {
	return childrenOfWithState(pid, "Z")
}

// childrenOfWithState returns the PIDs of all processes whose parent is pid
// and whose state starts with state, any state if empty.
func childrenOfWithState(pid int, state string) []int {
	const procPath = "/proc"

	entries, err := os.ReadDir(procPath)
	Expect(err).To(Succeed())

	ppid := "PPid:\t" + strconv.Itoa(pid) + "\n"
	stateLine := "State:\t" + state

	var children []int

	for _, entry := range entries {
		child, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}

		// The process may be gone already.
		status, err := os.ReadFile(filepath.Join(procPath, entry.Name(), "status"))
		if err == nil && bytes.Contains(status, []byte(ppid)) && bytes.Contains(status, []byte(stateLine)) {
			children = append(children, child)
		}
	}

	return children
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

func (tr *testRunner) startContainer() {
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

// syncBuffer is a bytes.Buffer which can be read while another goroutine
// still writes to it.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()

	n, err := b.buf.Write(p)
	if err != nil {
		return n, fmt.Errorf("write buffer: %w", err)
	}

	return n, nil
}

func (b *syncBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()

	return b.buf.String()
}

// cleanupSpec deletes the container, shuts the server down and removes the
// temp dir of a spec, whatever the spec got around to create.
func cleanupSpec(tr *testRunner, sut *client.ConmonClient) {
	// Every step runs even if an earlier one failed, a leaked server would
	// otherwise fail the whole suite.
	var errs []error

	// The runner only exists once the runtime config got created.
	if tr != nil && tr.rr != nil {
		if err := tr.rr.RunCommand("delete", "-f", tr.ctrID); err != nil {
			errs = append(errs, fmt.Errorf("delete container: %w", err))
		}
	}

	if sut != nil {
		if err := sut.Shutdown(); err != nil {
			errs = append(errs, fmt.Errorf("shutdown server: %w", err))
		}
	}

	if tr != nil {
		if err := os.RemoveAll(tr.tmpDir); err != nil {
			errs = append(errs, fmt.Errorf("remove temp dir: %w", err))
		}
	}

	Expect(errors.Join(errs...)).To(Succeed())
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
	Expect(err).To(Succeed())
	Expect(f.Close()).To(Succeed())

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

// cacheBusyBox downloads busybox once per user. A provided BUSYBOX_BINARY
// is used as it is, a cached file is checked and replaced if it does not
// match, an earlier failed download must not stay in use.
func cacheBusyBox() error {
	if os.Getenv("BUSYBOX_BINARY") != "" {
		if _, err := os.Stat(busyboxDest); err != nil {
			return fmt.Errorf("%w: %s", errBusyboxMissing, busyboxDest)
		}

		return nil
	}

	if sum, err := fileSHA256(busyboxDest); err == nil && sum == busyboxSHA256 {
		return nil
	}

	if err := os.MkdirAll(busyboxDestDir, 0o755); err != nil {
		return fmt.Errorf("create busybox dest dir: %w", err)
	}

	if err := downloadFile(busyboxSource, busyboxDest, busyboxSHA256); err != nil {
		return fmt.Errorf("download busybox: %w", err)
	}

	return nil
}

// fileSHA256 returns the hex checksum of a file.
func fileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open file: %w", err)
	}
	defer f.Close()

	hash := sha256.New()
	if _, err := io.Copy(hash, f); err != nil {
		return "", fmt.Errorf("read file: %w", err)
	}

	return hex.EncodeToString(hash.Sum(nil)), nil
}

// linkOrCopy hard links src to dst and copies it if the link is not
// possible, for example across file systems.
func linkOrCopy(src, dst string) error {
	if err := os.Link(src, dst); err == nil {
		return nil
	}

	in, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open source: %w", err)
	}
	defer in.Close()

	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o755)
	if err != nil {
		return fmt.Errorf("create destination: %w", err)
	}
	defer out.Close()

	if _, err := io.Copy(out, in); err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

// downloadFile downloads a URL into path. The file only appears at path once
// it is complete and matches the checksum, a failed download must not get
// cached.
func downloadFile(url, path, sha256sum string) error {
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

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%w: %s", errDownloadStatus, resp.Status)
	}

	out, err := os.CreateTemp(filepath.Dir(path), "download-")
	if err != nil {
		return fmt.Errorf("create temp file: %w", err)
	}

	defer os.Remove(out.Name())
	defer out.Close()

	hash := sha256.New()
	if _, err := io.Copy(io.MultiWriter(out, hash), resp.Body); err != nil {
		return fmt.Errorf("copy response: %w", err)
	}

	if sum := hex.EncodeToString(hash.Sum(nil)); sum != sha256sum {
		return fmt.Errorf("%w: %s is %s, expected %s", errChecksumMismatch, url, sum, sha256sum)
	}

	if err := out.Chmod(0o755); err != nil {
		return fmt.Errorf("change permissions: %w", err)
	}

	if err := out.Close(); err != nil {
		return fmt.Errorf("close file: %w", err)
	}

	if err := os.Rename(out.Name(), path); err != nil {
		return fmt.Errorf("move file into place: %w", err)
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
	// conmonrs runs with an OOM score of -1000 and the runtime inherits it.
	// Without an explicit value the container would be unkillable and hit
	// allocation errors instead of the OOM killer, like CRI-O always sets one.
	g.SetProcessOOMScoreAdj(0)
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

	//nolint:gosec // test helper with controlled inputs
	cmd := exec.CommandContext(context.Background(), runtimePath, append(rr.runtimeRootArgs(), args...)...)
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
		// Nothing reads the output once this returns, closing the reader
		// unblocks the attach if it still writes, also after a failed
		// assertion.
		defer reader.Close()

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
	}()

	go func() {
		defer wg.Done()
		defer GinkgoRecover()

		err := sut.AttachContainer(context.Background(), cfg)
		// The attach may still be writing output when the reader gets
		// closed above, that is the only error allowed.
		if !errors.Is(err, io.ErrClosedPipe) {
			Expect(err).To(Succeed())
		}
	}()

	wg.Wait()
}

func verifyBuffer(reader io.Reader, terminal bool, command, expected string) {
	// A read blocks on the pipe until output arrives, which never happens
	// after a mismatch, so bound it.
	readSection := func() string {
		type result struct {
			data string
			err  error
		}

		results := make(chan result, 1)

		go func() {
			data := make([]byte, 8191)

			n, err := reader.Read(data)
			results <- result{string(data[:n]), err}
		}()

		select {
		case res := <-results:
			Expect(res.err).To(Succeed())

			return res.data
		case <-time.After(10 * time.Second):
			Fail("timed out waiting for attach output")

			return ""
		}
	}
	if !terminal {
		Expect(readSection()).To(Equal(expected))

		return
	}

	// The terminal echoes the command and prints the prompt, which arrive
	// in any number of reads.
	fullExpectedBuffer := command + "\r\n" + expected + "/ # \x1b[6n"
	str := ""

	for len(str) < len(fullExpectedBuffer) {
		str += readSection()
	}

	Expect(str).To(Equal(fullExpectedBuffer))
}
