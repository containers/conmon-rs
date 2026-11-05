package client_test

import (
	"fmt"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/google/uuid"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
)

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

		It("should write the container name", func(ctx SpecContext) {
			tr = newTestRunner()
			marker := uuid.New().String()
			tr.createRuntimeConfigWithProcessArgs(
				false,
				[]string{"/busybox", "echo", "hello " + marker},
				nil,
			)

			sut = tr.configGivenEnv()

			cfg := tr.defaultConfig(false)
			cfg.LogDrivers = []client.ContainerLogDriver{{
				Type:          client.LogDriverTypeJournald,
				ContainerName: "name-" + marker,
			}}
			tr.createContainerWithConfig(ctx, sut, cfg)
			tr.startContainer()

			// The matches of different fields all have to hold, so the line
			// only shows up with every field set. The name is the identifier
			// as well.
			args := []string{
				"--since", "-1min",
				"CONTAINER_ID=" + tr.ctrID[:12],
				"CONTAINER_NAME=name-" + marker,
				"SYSLOG_IDENTIFIER=name-" + marker,
			}
			Eventually(journalctl(ctx, args...), 10*time.Second).
				Should(ContainSubstring("hello " + marker))
		}, SpecTimeout(specTimeout))
	})
})
