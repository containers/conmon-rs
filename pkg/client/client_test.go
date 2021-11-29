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
	. "github.com/onsi/ginkgo"
	. "github.com/onsi/gomega"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-tools/generate"
)

const (
	maxRSSKB       = 3200
	busyboxSource  = "https://busybox.net/downloads/binaries/1.31.0-i686-uclibc/busybox"
	busyboxDestDir = "/tmp/conmon-test-images"
)

var (
	busyboxDest = filepath.Join(busyboxDestDir, "busybox")
	runtimePath = os.Getenv("RUNTIME_BINARY")
	conmonPath  = os.Getenv("CONMON_BINARY")
)

// TestConmonClient runs the created specs
func TestConmonClient(t *testing.T) {
	RegisterFailHandler(Fail)
	RunSpecs(t, "ConmonClient")
}

var _ = Describe("ConmonClient", func() {
	var (
		tmpDir, pidFilePath, socketPath, tmpRootfs, ctrID string
		rr                                                *RuntimeRunner
	)

	var sut *client.ConmonClient
	BeforeEach(func() {
		tmpDir = MustTempDir("conmon-client")
		pidFilePath = MustFileInTempDir(tmpDir, "pidfile")
		socketPath = MustFileInTempDir(tmpDir, "socket")
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
		Expect(generateRuntimeConfig(tmpDir, tmpRootfs)).To(BeNil())
	})

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
		It("Should create a simple container", func() {
			sut = configGivenEnv(socketPath, pidFilePath, rr.runtimeRoot)
			Expect(WaitUntilServerUp(sut)).To(BeNil())
			pid, err := sut.CreateContainer(context.Background(), &client.CreateContainerConfig{
				ID:         ctrID,
				BundlePath: tmpDir,
			})
			Expect(err).To(BeNil())
			Expect(pid).NotTo(Equal(0))
			Eventually(func() error {
				return rr.RunCommandCheckOutput(ctrID, "list")
			}, time.Second*5).Should(BeNil())
		})
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

func WaitUntilServerUp(sut *client.ConmonClient) error {
	var err error
	for i := 0; i < 100; i++ {
		_, err = sut.Version(context.Background())
		if err == nil {
			break
		}
		time.Sleep(1 * time.Millisecond)
	}
	return err
}

func configGivenEnv(socketPath, pidFilePath, runtimeRoot string) *client.ConmonClient {
	sut, err := client.New(&client.ConmonServerConfig{
		ConmonPIDFile:    pidFilePath,
		Runtime:          runtimePath,
		Socket:           socketPath,
		ConmonServerPath: conmonPath,
		Stdin:            os.Stdin,
		Stdout:           os.Stdout,
		Stderr:           os.Stderr,
		RuntimeRoot:      runtimeRoot,
		LogLevel:         "debug",
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
	resp, err := http.Get(url)
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

func generateRuntimeConfig(bundlePath, rootfs string) error {
	configPath := filepath.Join(bundlePath, "config.json")
	g, err := generate.New("linux")
	if err != nil {
		return err
	}
	g.SetProcessCwd("/")
	g.SetProcessArgs([]string{"/busybox", "ls"})
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
