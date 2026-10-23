package client_test

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	neturl "net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime/pprof"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/gorilla/websocket"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-tools/generate"
	"github.com/sirupsen/logrus"
	"go.podman.io/storage/pkg/stringid"
	"go.podman.io/storage/pkg/unshare"
	"golang.org/x/sys/unix"
	"k8s.io/apimachinery/pkg/util/proxy"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/portforward"
	"k8s.io/client-go/tools/remotecommand"
	"k8s.io/client-go/transport/spdy"
	"k8s.io/streaming/pkg/httpstream"
)

const (
	timeoutUnlimited = 0
	conmonBinaryKey  = "CONMON_BINARY"

	// specTimeout bounds a spec, so that a call which hangs fails it with
	// a stack, not the whole suite.
	specTimeout = 2 * time.Minute

	// sttyLoop prints the size of the terminal, the rows and then the
	// columns, until it gets killed.
	sttyLoop = "while true; do /busybox stty size; /busybox sleep 0.1; done"

	// sttyAwait waits until the terminal has 40 rows and 30 columns, then
	// prints its size.
	sttyAwait = `while [ "$(/busybox stty size)" != "40 30" ]; do /busybox sleep 0.1; done; ` +
		"/busybox stty size"
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
	// How much the resident set of the server may grow over the execs of the
	// memory leak specs, MAX_RSS_GROWTH_KB overrides it.
	maxRSSGrowthKB = 2000
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

	if growthStr := os.Getenv("MAX_RSS_GROWTH_KB"); growthStr != "" {
		growthInt, err := strconv.Atoi(growthStr)
		if err != nil {
			t.Fatalf("parse MAX_RSS_GROWTH_KB: %v", err)
		}

		maxRSSGrowthKB = growthInt
	}

	if binary := os.Getenv("BUSYBOX_BINARY"); binary != "" {
		busyboxDest = binary
	}

	// Set once, goroutines of earlier specs may still log.
	logrus.SetLevel(logrus.TraceLevel)

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
	return pauseProcessCount(podID) > 0
}

// pauseProcessCount returns the number of conmonrs pause processes of the pod.
func pauseProcessCount(podID string) int {
	cmd := exec.CommandContext(context.Background(), "ps", "-eo", "args")

	var stdout bytes.Buffer

	cmd.Stdout = &stdout
	Expect(cmd.Run()).To(Succeed())

	count := 0

	for line := range strings.SplitSeq(stdout.String(), "\n") {
		if strings.Contains(line, " pause ") && strings.Contains(line, podID) {
			count++
		}
	}

	return count
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
		if err == nil && bytes.Contains(status, []byte(ppid)) &&
			bytes.Contains(status, []byte(stateLine)) {
			children = append(children, child)
		}
	}

	return children
}

// pidsWithCmdline returns the processes which run exactly the command.
func pidsWithCmdline(command []string) []int {
	const procPath = "/proc"

	entries, err := os.ReadDir(procPath)
	Expect(err).To(Succeed())

	want := []byte(strings.Join(command, "\x00") + "\x00")

	var pids []int

	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}

		// The process may be gone already.
		cmdline, err := os.ReadFile(filepath.Join(procPath, entry.Name(), "cmdline"))
		if err == nil && bytes.Equal(cmdline, want) {
			pids = append(pids, pid)
		}
	}

	return pids
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

// logLineNumbers returns the numbers of the CRI log lines "line <n>" in the
// file at path, nil if the file does not exist. A line being written fails
// the assertions of g, so that an Eventually retries.
func logLineNumbers(g Gomega, path string) []int {
	contents, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}

	g.Expect(err).To(Succeed())

	var numbers []int

	for line := range strings.Lines(string(contents)) {
		// <timestamp> <pipe> <tag> line <n>, a terminal ends it with "\r".
		fields := strings.Fields(line)
		g.Expect(fields).To(HaveLen(5), "unexpected log line %q", line)
		g.Expect(fields[3]).To(Equal("line"))

		number, err := strconv.Atoi(fields[4])
		g.Expect(err).To(Succeed())

		numbers = append(numbers, number)
	}

	return numbers
}

// fileContents reads a file, also for polling with Eventually: a file which
// does not exist yet gets read again, any other error stops the polling right
// away.
func fileContents(path string) (string, error) {
	contents, err := os.ReadFile(path)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return "", StopTrying("read " + path).Wrap(err)
	}

	return string(contents), err
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

func (tr *testRunner) createContainer(
	ctx context.Context,
	sut *client.ConmonClient,
	terminal bool,
) {
	tr.createContainerWithConfig(ctx, sut, tr.defaultConfig(terminal))
}

func (tr *testRunner) createContainerWithConfig(
	ctx context.Context,
	sut *client.ConmonClient,
	cfg *client.CreateContainerConfig,
) {
	resp, err := sut.CreateContainer(ctx, cfg)
	Expect(err).To(Succeed())
	Expect(resp.PID).NotTo(BeEquivalentTo(0))
	//nolint:contextcheck // the runtime commands do not hang, the polling is bounded
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
	return tr.configGivenEnvWith(nil)
}

// configGivenEnvWith starts a server with the configuration of
// configGivenEnv, changed by changeConfig if not nil.
func (tr *testRunner) configGivenEnvWith(
	changeConfig func(*client.ConmonServerConfig),
) *client.ConmonClient {
	cfg := client.NewConmonServerConfig(runtimePath, tr.rr.runtimeRoot, tr.tmpDir)
	cfg.ConmonServerPath = conmonPath
	cfg.LogDriver = client.LogDriverStdout

	cfg.ClientLogger = logrus.StandardLogger()

	if tr.enableTracing {
		cfg.Tracing = &client.Tracing{Enabled: true}
	}

	if changeConfig != nil {
		changeConfig(cfg)
	}

	sut, err := client.New(cfg)
	Expect(err).To(Succeed())
	Expect(sut).NotTo(BeNil())

	return sut
}

// logMemoryDetails prints the threads and the resident memory by kind of a
// process, which tells a leak from memory the allocator or the kernel keeps.
func logMemoryDetails(pid uint32, when string) {
	for _, file := range []string{"status", "smaps_rollup"} {
		content, err := os.ReadFile(fmt.Sprintf("/proc/%d/%s", pid, file))
		Expect(err).To(Succeed())

		for line := range strings.SplitSeq(string(content), "\n") {
			for _, key := range []string{"Threads:", "Rss:", "Anonymous:", "AnonHugePages:", "Private_Dirty:"} {
				if strings.HasPrefix(line, key) {
					GinkgoWriter.Printf(
						"Memory %s: %s\n",
						when,
						strings.Join(strings.Fields(line), " "),
					)
				}
			}
		}
	}
}

// mappingRSS returns the resident kB of each mapping of a process by its start
// address, permissions and name, so that a heap grown through brk stays the
// same mapping.
func mappingRSS(pid uint32) map[string]int {
	content, err := os.ReadFile(fmt.Sprintf("/proc/%d/smaps", pid))
	Expect(err).To(Succeed())

	mappings := map[string]int{}
	mapping := ""

	for line := range strings.SplitSeq(string(content), "\n") {
		fields := strings.Fields(line)
		if len(fields) == 0 {
			continue
		}

		switch {
		case strings.Contains(fields[0], "-") && !strings.HasSuffix(fields[0], ":"):
			start, _, _ := strings.Cut(fields[0], "-")
			mapping = strings.Join(append([]string{start, fields[1]}, fields[5:]...), " ")
		case fields[0] == "Rss:" && len(fields) > 1:
			kb, err := strconv.Atoi(fields[1])
			Expect(err).To(Succeed())

			mappings[mapping] = kb
		}
	}

	return mappings
}

// logGrownMappings prints the mappings which grew by more than 256 kB.
func logGrownMappings(before, after map[string]int) {
	for mapping, kb := range after {
		if grown := kb - before[mapping]; grown > 256 {
			GinkgoWriter.Printf("Mapping grew by %d kB to %d kB: %s\n", grown, kb, mapping)
		}
	}
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

// serverResources are the open file descriptors and the threads of the
// server.
type serverResources struct {
	fds, threads int
}

// resourcesOf counts the open file descriptors and the threads of a process.
func resourcesOf(pid uint32) serverResources {
	count := func(dir string) int {
		entries, err := os.ReadDir(fmt.Sprintf("/proc/%d/%s", pid, dir))
		Expect(err).To(Succeed())

		return len(entries)
	}

	return serverResources{fds: count("fd"), threads: count("task")}
}

// withoutDeadline returns a context which ends with ctx but has no deadline.
// The deadline of a SpecContext would otherwise become the timeout of an
// ExecSync with timeoutUnlimited.
func withoutDeadline(ctx context.Context) context.Context {
	noDeadline, cancel := context.WithCancel(context.WithoutCancel(ctx))
	stop := context.AfterFunc(ctx, cancel)

	DeferCleanup(func() {
		stop()
		cancel()
	})

	return noDeadline
}

// expectNoResourceLeak waits until the server holds no more file descriptors
// and threads than before. The threads of the blocking pool of the server
// linger for 10 seconds once they are idle, so that may take a while.
func expectNoResourceLeak(pid uint32, before serverResources) {
	Eventually(func(g Gomega) {
		now := resourcesOf(pid)
		GinkgoWriter.Printf("Server resources %+v, before %+v\n", now, before)
		g.Expect(now.fds).To(BeNumerically("<=", before.fds), "open file descriptors")
		g.Expect(now.threads).To(BeNumerically("<=", before.threads), "threads")
	}).WithTimeout(45 * time.Second).WithPolling(500 * time.Millisecond).Should(Succeed())
}

// closedPort returns a local port which refuses connections. Unlike the
// port of a closed listener, nothing else can take it while the spec runs: a
// socket stays bound to it without listening until the spec ends.
func closedPort() int {
	fd, err := unix.Socket(unix.AF_INET, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	Expect(err).To(Succeed())
	DeferCleanup(unix.Close, fd)

	Expect(unix.Bind(fd, &unix.SockaddrInet4{Addr: [4]byte{127, 0, 0, 1}})).To(Succeed())

	addr, err := unix.Getsockname(fd)
	Expect(err).To(Succeed())

	inet4, ok := addr.(*unix.SockaddrInet4)
	Expect(ok).To(BeTrue())

	return inet4.Port
}

// sizeQueue hands the terminal sizes sent to it to a streaming client, until
// it gets closed.
type sizeQueue chan remotecommand.TerminalSize

// terminalSizes returns a closed queue with the sizes.
func terminalSizes(sizes ...remotecommand.TerminalSize) sizeQueue {
	queue := make(sizeQueue, len(sizes))
	for _, size := range sizes {
		queue <- size
	}

	close(queue)

	return queue
}

func (q sizeQueue) Next() *remotecommand.TerminalSize {
	size, ok := <-q
	if !ok {
		return nil
	}

	return &size
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

var (
	errNoMatch  = errors.New("regex does not match")
	errNotTCP   = errors.New("not a TCP connection")
	errProtocol = errors.New("unexpected port forward protocol")
)

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
	cmd := exec.CommandContext(
		context.Background(),
		runtimePath,
		append(rr.runtimeRootArgs(), args...)...)
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
	ctx context.Context,
	sut *client.ConmonClient,
	cfg *client.AttachConfig,
	stdinWriter io.Writer,
	reader *io.PipeReader,
	testString string,
	useStdErr bool,
	terminal bool,
) {
	command := "/busybox echo -n " + testString

	attachErr := make(chan error, 1)

	go func() {
		attachErr <- sut.AttachContainer(ctx, cfg)
	}()

	func() {
		// Once the output got verified, the rest of it, like the echo of
		// the exit of a terminal, is drained until the attach closes the
		// stream. After a failed assertion the reader gets closed instead,
		// which ends an attach that still writes.
		verified := false

		defer func() {
			if verified {
				go io.Copy(io.Discard, reader) //nolint:errcheck // only drains
			} else {
				reader.Close()
			}
		}()

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

		verified = true
	}()

	Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
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

	// The terminal echoes the command and prints the prompt. The busybox of
	// the tests asks the terminal for the cursor position after it, the one
	// of nixpkgs for other architectures shows ~ for the home directory of
	// root instead and does not ask. Either way the output arrives in any
	// number of reads.
	fullExpectedBuffers := []string{expected}
	if terminal {
		fullExpectedBuffers = []string{
			command + "\r\n" + expected + "/ # \x1b[6n",
			command + "\r\n" + expected + "~ # ",
		}
	}

	maxLen := 0
	for _, buf := range fullExpectedBuffers {
		maxLen = max(maxLen, len(buf))
	}

	str := ""

	for !slices.Contains(fullExpectedBuffers, str) && len(str) < maxLen {
		str += readSection()
	}

	Expect(str).To(BeElementOf(fullExpectedBuffers))
}

// streamTransports are the transports of exec and attach sessions: WebSocket
// with v5.channel.k8s.io like kubectl, and SPDY like the kubelet towards the
// runtime.
var streamTransports = []string{"WebSocket", "SPDY"}

// streamURLOver runs the exec or attach session served at url over the
// transport until it ends.
func streamURLOver(
	ctx context.Context,
	transport, rawURL string,
	options remotecommand.StreamOptions,
) error {
	var (
		executor remotecommand.Executor
		err      error
	)

	if transport == "SPDY" {
		parsed, parseErr := neturl.Parse(rawURL)
		Expect(parseErr).NotTo(HaveOccurred())

		executor, err = remotecommand.NewSPDYExecutor(&rest.Config{}, http.MethodPost, parsed)
	} else {
		executor, err = remotecommand.NewWebSocketExecutor(
			&rest.Config{Insecure: true},
			http.MethodGet,
			rawURL,
		)
	}

	Expect(err).NotTo(HaveOccurred())

	if err := executor.StreamWithContext(
		ctx,
		options,
	); err != nil {
		return fmt.Errorf("create stream with context: %w", err)
	}

	return nil
}

// legacyExec runs the exec session served at url with a WebSocket protocol
// before v5, which has no close signal for stdin. It returns what the client
// received on stdout, stderr and the error channel until the server closed
// the session.
func legacyExec(rawURL, protocol string) (stdout, stderr, status string, err error) {
	base64Codec := strings.Contains(protocol, "base64")
	wsURL := strings.Replace(rawURL, "http://", "ws://", 1)
	dialer := websocket.Dialer{Subprotocols: []string{protocol}, HandshakeTimeout: 5 * time.Second}

	conn, resp, err := dialer.Dial(wsURL, nil)
	if resp != nil {
		resp.Body.Close()
	}

	if err != nil {
		return "", "", "", fmt.Errorf("dial: %w", err)
	}

	defer conn.Close()

	if conn.Subprotocol() != protocol {
		return "", "", "", fmt.Errorf("%w: %q", errProtocol, conn.Subprotocol())
	}

	received := map[byte]string{}

	for {
		if err := conn.SetReadDeadline(time.Now().Add(10 * time.Second)); err != nil {
			return "", "", "", fmt.Errorf("set deadline: %w", err)
		}

		_, message, err := conn.ReadMessage()
		if websocket.IsCloseError(err, websocket.CloseNormalClosure) {
			break
		}

		if err != nil {
			return "", "", "", fmt.Errorf("read message: %w", err)
		}

		channel, content := message[0], message[1:]
		if base64Codec {
			channel -= '0'

			if content, err = base64.StdEncoding.DecodeString(string(content)); err != nil {
				return "", "", "", fmt.Errorf("decode message: %w", err)
			}
		}

		received[channel] += string(content)
	}

	return received[1], received[2], received[3], nil
}

// portForwardDialer dials the port forward session served at url over SPDY,
// like the kubelet does.
//
//nolint:ireturn // client-go only exposes the interface
func portForwardDialer(rawURL string) (httpstream.Dialer, error) {
	parsed, err := neturl.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("parse url: %w", err)
	}

	transport, upgrader, err := spdy.RoundTripperFor(&rest.Config{})
	if err != nil {
		return nil, fmt.Errorf("create round tripper: %w", err)
	}

	return spdy.NewDialerForStreaming(
		upgrader,
		&http.Client{Transport: transport},
		http.MethodPost,
		parsed,
	), nil
}

// tunnelDialer dials the port forward session served at url with SPDY
// tunneled through a WebSocket, like crictl and critest do with the websocket
// transport.
//
//nolint:ireturn // client-go only exposes the interface
func tunnelDialer(rawURL string) (httpstream.Dialer, error) {
	parsed, err := neturl.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("parse url: %w", err)
	}

	dialer, err := portforward.NewSPDYOverWebsocketDialerForStreaming(parsed, &rest.Config{})
	if err != nil {
		return nil, fmt.Errorf("create tunneling dialer: %w", err)
	}

	return dialer, nil
}

// startPortForward forwards a free local port to the port of the session the
// dialer dials, with the port forwarder of client-go. The stop function ends
// the forwarding and waits for it.
func startPortForward(dialer httpstream.Dialer, port int) (local uint16, stop func(), err error) {
	stopChan := make(chan struct{})
	readyChan := make(chan struct{})

	forwarder, err := portforward.NewOnAddressesForStreaming(
		dialer, []string{"127.0.0.1"}, []string{fmt.Sprintf("0:%d", port)},
		stopChan, readyChan, GinkgoWriter, GinkgoWriter,
	)
	if err != nil {
		return 0, nil, fmt.Errorf("create port forwarder: %w", err)
	}

	errChan := make(chan error, 1)

	go func() { errChan <- forwarder.ForwardPorts() }()

	select {
	case <-readyChan:
	case forwardErr := <-errChan:
		return 0, nil, fmt.Errorf("forward ports: %w", forwardErr)
	}

	stop = func() {
		close(stopChan)

		if forwardErr := <-errChan; forwardErr != nil {
			GinkgoWriter.Printf("Port forwarding ended: %v\n", forwardErr)
		}
	}

	ports, err := forwarder.GetPorts()
	if err != nil || len(ports) != 1 {
		stop()

		return 0, nil, fmt.Errorf("get forwarded ports %v: %w", ports, err)
	}

	return ports[0].Local, stop, nil
}

// portForwardRoundTrip sends the payload to the forwarded local port, closes
// the sending side and returns everything that came back until the remote
// side closed the connection.
func portForwardRoundTrip(local uint16, payload string) (string, error) {
	dialer := net.Dialer{Timeout: 5 * time.Second}

	conn, err := dialer.DialContext(context.Background(), "tcp", fmt.Sprintf("127.0.0.1:%d", local))
	if err != nil {
		return "", fmt.Errorf("dial forwarded port: %w", err)
	}
	defer conn.Close()

	if err = conn.SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		return "", fmt.Errorf("set deadline: %w", err)
	}

	if _, err = conn.Write([]byte(payload)); err != nil {
		return "", fmt.Errorf("write payload: %w", err)
	}

	tcpConn, ok := conn.(*net.TCPConn)
	if !ok {
		return "", fmt.Errorf("%w: %T", errNotTCP, conn)
	}

	if err = tcpConn.CloseWrite(); err != nil {
		return "", fmt.Errorf("close sending side: %w", err)
	}

	response, err := io.ReadAll(conn)
	if err != nil {
		return "", fmt.Errorf("read response: %w", err)
	}

	return string(response), nil
}

// echoServer listens on a free local port and echoes every connection. With
// a size of 0 until the client closes its sending side, otherwise the size
// in bytes, then it closes the connection itself. The channel protocol cannot
// close the sending side, so its server has to end the connection.
func echoServer(size int64) (listener net.Listener, port int) {
	listener, err := (&net.ListenConfig{}).Listen(context.Background(), "tcp4", "127.0.0.1:0")
	Expect(err).NotTo(HaveOccurred())

	addr, ok := listener.Addr().(*net.TCPAddr)
	Expect(ok).To(BeTrue())

	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}

			go func() {
				defer conn.Close()

				var err error
				if size > 0 {
					_, err = io.CopyN(conn, conn, size)
				} else {
					_, err = io.Copy(conn, conn)
				}

				if err != nil {
					GinkgoWriter.Printf("Echo failed: %v\n", err)
				}
			}()
		}
	}()

	return listener, addr.Port
}

// channelRoundTrip forwards the port over the WebSocket channel protocol,
// sends the payload on the data channel and returns what the data channel
// received until the server closed the session, and the error channel.
func channelRoundTrip(
	rawURL string,
	port int,
	base64Codec bool,
	payload string,
) (data, errs string, err error) {
	wsURL := strings.Replace(rawURL, "http://", "ws://", 1) + "?port=" + strconv.Itoa(port)

	protocol, messageType := "v4.channel.k8s.io", websocket.BinaryMessage
	if base64Codec {
		protocol, messageType = "v4.base64.channel.k8s.io", websocket.TextMessage
	}

	dialer := websocket.Dialer{Subprotocols: []string{protocol}, HandshakeTimeout: 5 * time.Second}

	conn, resp, err := dialer.Dial(wsURL, nil)
	if resp != nil {
		resp.Body.Close()
	}

	if err != nil {
		return "", "", fmt.Errorf("dial: %w", err)
	}

	defer conn.Close()

	if conn.Subprotocol() != protocol {
		return "", "", fmt.Errorf("%w: %q", errProtocol, conn.Subprotocol())
	}

	encode := func(channel byte, data []byte) []byte {
		if base64Codec {
			return append([]byte{'0' + channel}, base64.StdEncoding.EncodeToString(data)...)
		}

		return append([]byte{channel}, data...)
	}

	if err := conn.WriteMessage(messageType, encode(0, []byte(payload))); err != nil {
		return "", "", fmt.Errorf("send payload: %w", err)
	}

	received := map[byte][]byte{}

	for {
		if err := conn.SetReadDeadline(time.Now().Add(10 * time.Second)); err != nil {
			return "", "", fmt.Errorf("set deadline: %w", err)
		}

		_, message, err := conn.ReadMessage()
		if websocket.IsCloseError(err, websocket.CloseNormalClosure) {
			break
		}

		if err != nil {
			return "", "", fmt.Errorf("read message: %w", err)
		}

		channel, content := message[0], message[1:]
		if base64Codec {
			channel -= '0'

			if content, err = base64.StdEncoding.DecodeString(string(content)); err != nil {
				return "", "", fmt.Errorf("decode message: %w", err)
			}
		}

		received[channel] = append(received[channel], content...)
	}

	// Both channels start with the port.
	prefix := string([]byte{byte(port), byte(port >> 8)})
	for channel := range byte(2) {
		if !strings.HasPrefix(string(received[channel]), prefix) {
			return "", "", fmt.Errorf(
				"%w: channel %d got %q",
				errProtocol,
				channel,
				received[channel],
			)
		}
	}

	return string(received[0][2:]), string(received[1][2:]), nil
}

// kubeletProxy serves the URL through the proxy the kubelet puts in front of
// the streaming server of the runtime, which forwards SPDY upgrades as they
// are. It returns the URL of the proxy and a function to stop it.
func kubeletProxy(rawURL string) (proxyURL string, stop func()) {
	location, err := neturl.Parse(rawURL)
	Expect(err).NotTo(HaveOccurred())

	server := httptest.NewServer(
		proxy.NewUpgradeAwareHandler(location, nil, false, true, proxyResponder{}),
	)

	return server.URL + location.Path, server.Close
}

// proxyResponder reports errors of the kubelet proxy, like the kubelet does.
type proxyResponder struct{}

func (proxyResponder) Error(w http.ResponseWriter, _ *http.Request, err error) {
	http.Error(w, err.Error(), http.StatusInternalServerError)
}
