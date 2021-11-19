package client_test

import (
	"bufio"
	"context"
	"io/ioutil"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo"
	. "github.com/onsi/gomega"
)

const maxRSSKB = 3200

// TestConmonClient runs the created specs
func TestConmonClient(t *testing.T) {
	RegisterFailHandler(Fail)
	RunSpecs(t, "ConmonClient")
}

var _ = Describe("ConmonClient", func() {
	var pidFilePath, socketPath string
	var sut *client.ConmonClient
	BeforeEach(func() {
		pidFile, err := ioutil.TempFile("", "pidfile")
		Expect(err).To(BeNil())
		pidFilePath = pidFile.Name()
		socket, err := ioutil.TempFile("", "socket")
		Expect(err).To(BeNil())
		socketPath = socket.Name()
	})

	AfterEach(func() {
		Expect(os.Remove(socketPath)).To(BeNil())
		if sut != nil {
			Expect(sut.Shutdown()).To(BeNil())
		}
	})

	Describe("ConmonClient", func() {
		It("should spawn a server with low enough memory", func() {
			var err error

			sut = configGivenEnv(socketPath, pidFilePath)

			for i := 0; i < 100; i++ {
				_, err = sut.Version(context.Background())
				if err == nil {
					break
				}
				time.Sleep(1 * time.Millisecond)
			}
			Expect(err).To(BeNil())

			pid := sut.PID()
			Expect(pid).To(BeNumerically(">", 0))

			Expect(vmRSSGivenPID(pid)).To(BeNumerically("<", maxRSSKB))
		})
	})
})

func configGivenEnv(socketPath, pidFilePath string) *client.ConmonClient {
	var conmonPath, runtimePath string
	if path := os.Getenv("CONMON_BINARY"); path != "" {
		conmonPath = path
	}
	if path := os.Getenv("RUNTIME_BINARY"); path != "" {
		runtimePath = path
	}
	sut, err := client.New(&client.ConmonServerConfig{
		ConmonPIDFile:    pidFilePath,
		Runtime:          runtimePath,
		Socket:           socketPath,
		ConmonServerPath: conmonPath,
		Stdin:            os.Stdin,
		Stdout:           os.Stdout,
		Stderr:           os.Stderr,
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
