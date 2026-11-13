package client_test

import (
	"bufio"
	"bytes"
	"cmp"
	"context"
	"errors"
	"fmt"
	"io"
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
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"github.com/onsi/gomega/types"
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

	// specTimeout bounds a spec, so that a call which hangs fails it with
	// a stack, not the whole suite.
	specTimeout = 2 * time.Minute

	// sttyLoop prints the size of the terminal, the rows and then the
	// columns, until it gets killed.
	sttyLoop = "while true; do /busybox stty size; /busybox sleep 0.1; done"

	// countingLoop prints "line <n>", counting from 0, ten times a second
	// until it gets killed, see logLineNumbers.
	countingLoop = "i=0; while true; do echo line $i; i=$((i+1)); /busybox sleep 0.1; done"

	// sttyAwait waits until the terminal has 40 rows and 30 columns, then
	// prints its size.
	sttyAwait = `while [ "$(/busybox stty size)" != "40 30" ]; do /busybox sleep 0.1; done; ` +
		"/busybox stty size"
)

var (
	runtimePath = os.Getenv("RUNTIME_BINARY")
	conmonPath  = os.Getenv(conmonBinaryKey)
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

	if anonStr := os.Getenv("MAX_RSS_ANON_KB"); anonStr != "" {
		anonInt, err := strconv.Atoi(anonStr)
		if err != nil {
			t.Fatalf("parse MAX_RSS_ANON_KB: %v", err)
		}

		maxAnonKB = anonInt
	}

	if growthStr := os.Getenv("MAX_RSS_GROWTH_KB"); growthStr != "" {
		growthInt, err := strconv.Atoi(growthStr)
		if err != nil {
			t.Fatalf("parse MAX_RSS_GROWTH_KB: %v", err)
		}

		maxAnonGrowthKB = growthInt
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

	// Without CONMON_BINARY the client runs the conmonrs of the PATH, an
	// empty substring would match every line.
	binary := cmp.Or(conmonPath, "conmonrs")

	scanner := bufio.NewScanner(strings.NewReader(stdout.String()))
	for scanner.Scan() {
		text := scanner.Text()
		if strings.Contains(text, conmonBinaryKey) {
			continue
		}

		Expect(text).NotTo(ContainSubstring(binary))
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

// execPIDFiles returns the PID files of execs left in the runtime directory.
func execPIDFiles(runtimeDir string) []string {
	files, err := filepath.Glob(filepath.Join(runtimeDir, "exec_*pid"))
	Expect(err).To(Succeed())

	return files
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

// atLeastLines matches at least n log line numbers, see logLineNumbers.
//
//nolint:ireturn // a matcher, like the ones of gomega
func atLeastLines(n int) types.GomegaMatcher {
	return WithTransform(
		func(lines []int) int { return len(lines) },
		BeNumerically(">=", n),
	)
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
		CommandArgs: []string{},
	}
}

// oomSpec limits the memory of the container to 32 MiB, so that a process
// which keeps allocating, like tail /dev/zero, gets OOM killed. The swap
// gets pinned as well, otherwise the cgroup swaps on hosts with swap instead
// of being killed. 32 MiB fit the init of runc, a Go program; tail grows
// beyond any limit.
func oomSpec(g generate.Generator) {
	const limit = 32 * 1024 * 1024
	g.SetLinuxResourcesMemoryLimit(limit)
	g.SetLinuxResourcesMemorySwap(limit)
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

// execEcho runs `echo -n hello world <i>` in the container of the runner
// with ExecSyncContainer and checks its output.
func (tr *testRunner) execEcho(
	ctx context.Context, sut *client.ConmonClient, terminal bool, i int,
) {
	result, err := sut.ExecSyncContainer(
		ctx,
		&client.ExecSyncContainerConfig{
			ID: tr.ctrID,
			Command: []string{
				"/busybox",
				"echo",
				"-n",
				"hello",
				"world",
				strconv.Itoa(i),
			},
			Terminal: terminal,
			Timeout:  timeoutUnlimited,
		},
	)
	Expect(err).To(Succeed())
	Expect(result).NotTo(BeNil())
	Expect(string(result.Stdout)).To(Equal(fmt.Sprintf("hello world %d", i)))
	GinkgoWriter.Println("done with", i, string(result.Stdout))
}

// journalctl returns a function which runs journalctl with the arguments
// and returns its output, for Eventually.
func journalctl(ctx context.Context, args ...string) func(g Gomega) string {
	return func(g Gomega) string {
		cmd := exec.CommandContext(ctx, "journalctl", args...)
		stdout := strings.Builder{}
		cmd.Stdout = &stdout
		g.Expect(cmd.Run()).To(Succeed())

		return stdout.String()
	}
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

// runtimeVersion is the output of the version command of the runtime, which
// runs once for all specs.
var runtimeVersion = sync.OnceValues(func() ([]byte, error) {
	return exec.Command(runtimePath, "--version").CombinedOutput()
})

// skipWithoutCheckpointSupport skips the spec if the runtime cannot checkpoint
// containers here: rootless, without criu, or with a crun which got built
// without CRIU support, like its static release binaries.
func skipWithoutCheckpointSupport() {
	if unshare.IsRootless() {
		Skip("does not run rootless")
	}

	if _, err := exec.LookPath("criu"); err != nil {
		Skip("criu is not installed")
	}

	version, err := runtimeVersion()
	Expect(err).To(Succeed(), string(version))

	if strings.HasPrefix(string(version), "crun") && !strings.Contains(string(version), "+CRIU") {
		Skip("crun got built without CRIU support")
	}
}

// skipWithoutJournald skips the spec if no journal runs here, like in a
// container or a nix sandbox: journalctl is missing or the socket of the
// journal, which the server and the drivers log to, does not exist. CI
// always has one.
func skipWithoutJournald(ctx context.Context) {
	if err := exec.CommandContext(ctx, "journalctl", "--version").Run(); err != nil {
		Skip("journalctl does not work: " + err.Error())
	}

	if _, err := os.Stat("/run/systemd/journal/socket"); err != nil {
		Skip("no journal runs: " + err.Error())
	}
}

// skipWithoutOOMDetection skips the spec if the OOM kill of a container
// cannot be told here: rootless, where the memory limit of the container
// needs permissions for the cgroups, or without the unified hierarchy of
// cgroup v2 at /sys/fs/cgroup, like with cgroup v1 or a hybrid setup, where
// the server does not watch for OOM kills. It is the check of the server.
func skipWithoutOOMDetection() {
	if unshare.IsRootless() {
		Skip("does not run rootless")
	}

	var cgroupRoot unix.Statfs_t
	if err := unix.Statfs("/sys/fs/cgroup", &cgroupRoot); err != nil {
		Skip("no cgroup v2 at /sys/fs/cgroup: " + err.Error())
	}

	if cgroupRoot.Type != unix.CGROUP2_SUPER_MAGIC {
		Skip("no cgroup v2 at /sys/fs/cgroup")
	}
}

// expectContainerStoppedOrGone waits until the runtime reports the killed
// container as stopped, or not at all once it got deleted.
func (tr *testRunner) expectContainerStoppedOrGone() {
	Eventually(func() bool {
		stopped := tr.rr.RunCommandCheckOutput("stopped", "list") == nil
		// Only a listing without the container counts, not a failed
		// listing.
		gone := errors.Is(tr.rr.RunCommandCheckOutput(tr.ctrID, "list"), errNoMatch)

		return stopped || gone
	}, 10*time.Second).Should(BeTrue(), "container still created")
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

// registerSpecHooks registers the hooks of a Describe whose specs keep their
// test runner in tr and their client in sut. They reset both before each
// spec, check the resident set size of the server after it and clean up. The
// reset keeps a spec which fails or skips before it created its server from
// making the hooks look at the server of the previous spec.
func registerSpecHooks(tr **testRunner, sut **client.ConmonClient) {
	BeforeEach(func() {
		*tr = nil
		*sut = nil
	})

	JustAfterEach(func() {
		expectRSSBelowCeiling(*sut)
	})

	AfterEach(func() {
		cleanupSpec(*tr, *sut)
	})
}

// containerRunner returns a function which starts a server with a started
// container which runs the process, for the specs whose hooks
// registerSpecHooks registered with tr and sut. Both get set before the
// steps which may fail, so that the hooks clean up what got created.
func containerRunner(
	tr **testRunner, sut **client.ConmonClient,
) func(ctx context.Context, terminal bool, processArgs []string) {
	//nolint:contextcheck // the helpers take no context, like in the specs
	return func(ctx context.Context, terminal bool, processArgs []string) {
		runner := newTestRunner()
		*tr = runner
		runner.createRuntimeConfigWithProcessArgs(terminal, processArgs, nil)
		*sut = runner.configGivenEnv()
		runner.createContainer(ctx, *sut, terminal)
		runner.startContainer()
	}
}

// serverStarter returns a function which starts a server for a container
// which runs ls without a terminal, for the specs whose hooks
// registerSpecHooks registered with tr and sut. The runner gets set before
// the steps which may fail, so that the hooks clean up what got created.
func serverStarter(tr **testRunner, sut **client.ConmonClient) func() {
	return func() {
		runner := newTestRunner()
		*tr = runner
		runner.createRuntimeConfig(false)
		*sut = runner.configGivenEnv()
	}
}

// serverOutput returns a file in the directory for the output of a server,
// which keeps writing to it once New returned, and a function which reads
// what got written so far.
func serverOutput(dir string) (file *os.File, read func() (string, error)) {
	file, err := os.CreateTemp(dir, "server-output-")
	Expect(err).To(Succeed())
	DeferCleanup(file.Close)

	return file, func() (string, error) {
		return fileContents(file.Name())
	}
}

// cleanupSpec deletes the container, shuts the server down, or kills it if
// it does not exit in time, closes the client and removes the temp dir of a
// spec, whatever the spec got around to create.
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
		if err := sut.Shutdown(context.Background()); err != nil {
			errs = append(errs, fmt.Errorf("shutdown server: %w", err))

			// Like CRI-O does it, a server which does not exit in time
			// gets killed.
			if errors.Is(err, client.ErrShutdownTimeout) {
				if err := sut.Kill(context.Background()); err != nil {
					errs = append(errs, fmt.Errorf("kill server: %w", err))
				}
			}
		}

		if err := sut.Close(); err != nil {
			errs = append(errs, fmt.Errorf("close client: %w", err))
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
	cfg := &client.ConmonServerConfig{
		ConmonServerPath: conmonPath,
		LogLevel:         client.LogLevelDebug,
		LogDriver:        client.LogDriverStdout,
		Runtime:          runtimePath,
		RuntimeRoot:      tr.rr.runtimeRoot,
		ServerRunDir:     tr.tmpDir,
		Stdout:           os.Stdout,
		Stderr:           os.Stderr,
	}

	cfg.Tracing.Enabled = tr.enableTracing

	if changeConfig != nil {
		changeConfig(cfg)
	}

	sut, err := client.New(cfg)
	Expect(err).To(Succeed())
	Expect(sut).NotTo(BeNil())

	return sut
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
	// CRI-O always sets an OOM score adjustment. Without one the container
	// keeps the score of the tests, which could be too low for the OOM
	// killer to pick it.
	g.SetProcessOOMScoreAdj(0)
	g.SetRootPath(rootfs)
	// The default seccomp profile of runtime-tools denies newer syscalls
	// with EPERM, like faccessat2, on which the init of runc 1.5 aborts. The
	// specs do not test seccomp, and CRI-O brings its own profile.
	g.Config.Linux.Seccomp = nil

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
	cmd := exec.CommandContext(
		context.Background(),
		runtimePath,
		append(rr.runtimeRootArgs(), args...)...)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("run command: %w: %s", err, strings.TrimSpace(stderr.String()))
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
	cfg *client.AttachContainerConfig,
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
		for i := range 2 {
			_, err := fmt.Fprintf(stdinWriter, "%s%s\n", command, pipe)
			Expect(err).To(Succeed())
			verifyBuffer(reader, terminal, i == 0, command, testString)
		}

		// terminate the container
		_, err := fmt.Fprintf(stdinWriter, "exit\n")
		Expect(err).To(Succeed())

		verified = true
	}()

	Eventually(attachErr, 10*time.Second).Should(Receive(Succeed()))
}

func verifyBuffer(reader io.Reader, terminal, first bool, command, expected string) {
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
		fullExpectedBuffers = terminalBuffers(first, command, expected)
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

// terminalBuffers returns the possible terminal outputs of a command, the
// echo, its output and the prompt.
func terminalBuffers(first bool, command, expected string) []string {
	prompts := []string{"/ # \x1b[6n", "~ # "}

	buffers := []string{}
	for _, prompt := range prompts {
		buffers = append(buffers, command+"\r\n"+expected+prompt)
	}

	// The first prompt goes to the log before attach clients, so an attach
	// right after it may still get it.
	if first {
		for _, before := range prompts {
			for _, after := range prompts {
				buffers = append(buffers, before+command+"\r\n"+expected+after)
			}
		}
	}

	return buffers
}
