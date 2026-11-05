package client_test

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/google/uuid"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
)

var _ = Describe("JSONLogger", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log in JSON format", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{"/busybox", "sh", "-c", "echo hello && echo world"},
					nil,
				)

				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers = []client.ContainerLogDriver{{
					Type: client.LogDriverTypeJSONLogger,
					Path: tr.logPath(),
				}}
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// The schema is documented in docs/logging.md.
				type entry struct {
					Timestamp uint64 `json:"timestamp"`
					Pipe      string `json:"pipe"`
					Message   string `json:"message"`
					Partial   bool   `json:"partial"`
				}

				var entries []entry

				Eventually(func(g Gomega) []entry {
					entries = nil

					contents, err := fileContents(tr.logPath())
					g.Expect(err).To(Succeed())

					for line := range strings.Lines(contents) {
						var e entry
						g.Expect(json.Unmarshal([]byte(line), &e)).To(Succeed(), "line %q", line)
						entries = append(entries, e)
					}

					return entries
				}, 10*time.Second).Should(HaveLen(2))

				for i, message := range []string{"hello", "world"} {
					Expect(entries[i].Timestamp).NotTo(BeZero())
					Expect(entries[i].Pipe).To(Equal("stdout"))
					// A terminal ends the lines with "\r\n", which is stripped as well.
					Expect(entries[i].Message).To(Equal(message))
					Expect(entries[i].Partial).To(BeFalse())
				}
			}, SpecTimeout(specTimeout))
		}
	})
})

var _ = Describe("JournaldLogger", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)

	Describe("Logging", func() {
		for _, terminal := range []bool{true, false} {
			It(testName("should log to journald", terminal), func(ctx SpecContext) {
				tr = newTestRunner()
				// Unique markers, other specs may log to the journal at the same time.
				marker := uuid.New().String()
				tr.createRuntimeConfigWithProcessArgs(
					terminal,
					[]string{
						"/busybox",
						"sh",
						"-c",
						"echo hello " + marker + " && echo foo " + marker + " >&2",
					},
					nil,
				)

				sut = tr.configGivenEnv()
				cfg := tr.defaultConfig(terminal)
				cfg.LogDrivers = []client.ContainerLogDriver{{Type: client.LogDriverTypeJournald}}
				tr.createContainerWithConfig(ctx, sut, cfg)
				tr.startContainer()

				// Verify the journal logs, they arrive asynchronously. The
				// documented short ID field finds both lines.
				journal := func(extraArgs ...string) func(g Gomega) string {
					return journalctl(ctx, append(
						[]string{"--since", "-1min", "CONTAINER_ID=" + tr.ctrID[:12]},
						extraArgs...)...)
				}
				Eventually(journal(), 10*time.Second).Should(And(
					ContainSubstring("hello "+marker),
					ContainSubstring("foo "+marker),
				))

				// Stderr goes out at error priority, stdout does not. A
				// terminal merges both into stdout.
				if !terminal {
					Eventually(journal("--priority", "err"), 10*time.Second).Should(And(
						Not(ContainSubstring("hello "+marker)),
						ContainSubstring("foo "+marker),
					))
				}
			}, SpecTimeout(specTimeout))
		}

		It("should log the server with the systemd driver", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)

			// The default of the Go client, which the other specs replace.
			sut = tr.configGivenEnvWith(func(cfg *client.ConmonServerConfig) {
				cfg.LogDriver = client.LogDriverSystemd
			})

			pid := sut.PID()
			Expect(pid).To(BeNumerically(">", 0))

			// An older server with the same PID must not match.
			journal := journalctl(
				ctx, "--since", "-1min", "-t", "conmonrs", fmt.Sprintf("_PID=%d", pid),
			)
			Eventually(journal, 10*time.Second).
				Should(ContainSubstring("Using systemd/journald logger"))
		}, SpecTimeout(specTimeout))

		It("should write the container name, tag and labels", func(ctx SpecContext) {
			tr = newTestRunner()
			marker := uuid.New().String()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "echo", "hello " + marker},
				nil,
			)

			sut = tr.configGivenEnv()
			Expect(sut.SupportsFeature(client.FeatureJournaldFields)).To(BeTrue())

			cfg := tr.defaultConfig(false)
			cfg.LogDrivers = []client.ContainerLogDriver{{
				Type:          client.LogDriverTypeJournald,
				ContainerName: "name-" + marker,
				Tag:           "tag-" + marker,
				Labels:        map[string]string{"CONMON_TEST_LABEL": "label-" + marker},
			}}
			tr.createContainerWithConfig(ctx, sut, cfg)
			tr.startContainer()

			// The matches of different fields all have to hold, so the line
			// only shows up with every field set. The tag names the
			// identifier as well.
			args := []string{
				"--since", "-1min",
				"CONTAINER_ID=" + tr.ctrID[:12],
				"CONTAINER_NAME=name-" + marker,
				"CONTAINER_TAG=tag-" + marker,
				"SYSLOG_IDENTIFIER=tag-" + marker,
				"CONMON_TEST_LABEL=label-" + marker,
			}
			Eventually(journalctl(ctx, args...), 10*time.Second).
				Should(ContainSubstring("hello " + marker))
		}, SpecTimeout(specTimeout))
	})
})
