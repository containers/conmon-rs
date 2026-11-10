package client_test

import (
	"bufio"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"golang.org/x/sys/unix"
)

var (
	// The budget of the Makefile for a dynamically linked release build,
	// MAX_RSS_KB overrides it. See the Makefile for the budgets.
	maxRSSKB = 10240
	// The budget of the private anonymous memory of the server (Anonymous of
	// its smaps_rollup), which the mapped binary and libraries do not count
	// towards, MAX_RSS_ANON_KB overrides it. The loose default of the
	// Makefile, CI sets a tighter one on amd64.
	maxAnonKB = 4096
	// How much the private anonymous memory of the server may grow over the
	// execs of the memory leak specs, MAX_RSS_GROWTH_KB overrides it.
	maxAnonGrowthKB = 256
)

// expectRSSBelowCeiling checks the resident set size of the server of a
// spec, if it got one, against maxRSSKB, and its private anonymous memory
// against maxAnonKB. Most of the resident set are the mapped binary and
// libraries, a regression of the memory the server allocates would hide in
// the slack of the total.
func expectRSSBelowCeiling(sut *client.ConmonClient) {
	if sut == nil {
		return
	}

	pid := sut.PID()
	Expect(pid).To(BeNumerically(">", 0))
	rss := statusKBGivenPID(pid, "VmRSS")
	anon := anonymousKBGivenPID(pid)
	// Named after the spec, so that a value can be attributed in the
	// verbose output of a whole run.
	GinkgoWriter.Printf(
		"VmRSS for server is %d kB, Anonymous %d kB in %q\n",
		rss,
		anon,
		CurrentSpecReport().FullText(),
	)
	Expect(rss).To(BeNumerically("<", maxRSSKB), "VmRSS of the server in kB")
	Expect(anon).To(BeNumerically("<", maxAnonKB), "Anonymous of the server in kB")
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

// statusKBGivenPID returns the value in kB of the field of the status of the
// process in /proc, like VmRSS.
func statusKBGivenPID(pid uint32, field string) int {
	return procKBGivenPID(pid, "status", field)
}

// anonymousKBGivenPID returns the private anonymous memory of the process in
// kB, summed up from its mappings. Unlike RssAnon of the status, which many
// kernels since 6.2 take from per CPU counters which can be off by hundreds
// of kB, it is exact. An int, so that the difference of two values cannot
// wrap.
func anonymousKBGivenPID(pid uint32) int {
	return procKBGivenPID(pid, "smaps_rollup", "Anonymous")
}

// procKBGivenPID returns the value in kB of the field of a file of the process
// in /proc with lines like "VmRSS: 1024 kB", as an int.
func procKBGivenPID(pid uint32, file, field string) int {
	const procPath = "/proc"

	procEntry := filepath.Join(procPath, strconv.Itoa(int(pid)), file)

	f, err := os.Open(procEntry)
	Expect(err).To(Succeed())

	defer f.Close()

	scanner := bufio.NewScanner(f)

	var value string

	for scanner.Scan() {
		parts := strings.Fields(scanner.Text())
		if len(parts) == 0 || parts[0] != field+":" {
			continue
		}

		Expect(parts).To(HaveLen(3))
		Expect(parts[2]).To(Equal("kB"))
		value = parts[1]

		break
	}

	Expect(scanner.Err()).To(Succeed())
	Expect(value).NotTo(BeEmpty(), "no %s in %s", field, procEntry)

	kb, err := strconv.Atoi(value)
	Expect(err).To(Succeed())

	return kb
}

// serverResources are the open file descriptors and the threads of the
// server, without the threads of its blocking pool.
type serverResources struct {
	fds, threads int
}

// blockingPoolThread is the name of the threads of the blocking pool of the
// server, the default of tokio. An idle one exits only after 10 seconds, so
// their number tells what the server did lately rather than what a spec
// leaked. If the check counted them, one in the baseline which exits in the
// meantime would hide a leaked thread of another kind, and a new one would
// make the check wait for it to exit. A blocking task which never ends still
// holds its file descriptors, which the check counts.
const blockingPoolThread = "tokio-rt-worker"

// resourcesOf counts the open file descriptors and the threads of a process,
// except for the ones of the blocking pool.
func resourcesOf(pid uint32) serverResources {
	fds, err := os.ReadDir(fmt.Sprintf("/proc/%d/fd", pid))
	Expect(err).To(Succeed())

	tasks, err := os.ReadDir(fmt.Sprintf("/proc/%d/task", pid))
	Expect(err).To(Succeed())

	threads := 0

	for _, task := range tasks {
		comm, err := os.ReadFile(fmt.Sprintf("/proc/%d/task/%s/comm", pid, task.Name()))
		// A thread which exited in the meantime does not count.
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, unix.ESRCH) {
			continue
		}

		Expect(err).To(Succeed())

		if strings.TrimSpace(string(comm)) != blockingPoolThread {
			threads++
		}
	}

	return serverResources{fds: len(fds), threads: threads}
}

// expectNoResourceLeak waits until the server holds no more file descriptors
// and threads than before, see resourcesOf for the threads it counts.
func expectNoResourceLeak(pid uint32, before serverResources) {
	Eventually(func(g Gomega) {
		now := resourcesOf(pid)
		GinkgoWriter.Printf("Server resources %+v, before %+v\n", now, before)
		g.Expect(now.fds).To(BeNumerically("<=", before.fds), "open file descriptors")
		g.Expect(now.threads).To(BeNumerically("<=", before.threads), "threads")
	}).WithTimeout(45 * time.Second).WithPolling(500 * time.Millisecond).Should(Succeed())
}
