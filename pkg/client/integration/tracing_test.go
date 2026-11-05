package client_test

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"strings"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)

	Describe("Tracing", func() {
		const contribTracingPath = "../../../contrib/tracing/"

		BeforeEach(func() {
			// Tracing is a compile time feature of the server, only a build
			// with it can export spans. Check before Jaeger gets started.
			binary := conmonPath
			if binary == "" {
				binary = "conmonrs"
			}

			version, err := exec.Command(binary, "--version-json", "full").Output()
			Expect(err).NotTo(HaveOccurred())

			if !strings.Contains(string(version), "opentelemetry") {
				Skip("server built without the tracing feature")
			}

			cmd := exec.Command(contribTracingPath + "start")

			fmt.Fprintln(os.Stdout)
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})

		hasTraces := func() bool {
			// Jaeger v2 only serves the v3 query API, which requires a time range.
			now := time.Now().UTC()
			query := url.Values{}
			query.Set("query.service_name", "conmonrs")
			query.Set("query.start_time_min", now.Add(-time.Hour).Format(time.RFC3339))
			query.Set("query.start_time_max", now.Add(time.Minute).Format(time.RFC3339))

			req, err := http.NewRequestWithContext(
				context.Background(), http.MethodGet,
				"http://localhost:16686/api/v3/traces?"+query.Encode(), http.NoBody,
			)
			Expect(err).NotTo(HaveOccurred())

			resp, err := http.DefaultClient.Do(req)
			Expect(err).NotTo(HaveOccurred())

			defer resp.Body.Close()

			// No matching trace is reported as not found.
			if resp.StatusCode == http.StatusNotFound {
				return false
			}

			Expect(resp.StatusCode).To(Equal(http.StatusOK))

			// The API streams one result object per chunk.
			type traceData struct {
				Result struct {
					ResourceSpans []any `json:"resourceSpans"`
				} `json:"result"`
			}

			decoder := json.NewDecoder(resp.Body)

			for {
				traces := &traceData{}

				err := decoder.Decode(traces)
				if errors.Is(err, io.EOF) {
					return false
				}

				Expect(err).NotTo(HaveOccurred())

				if len(traces.Result.ResourceSpans) > 0 {
					return true
				}
			}
		}

		It("should succeed", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			tr.enableTracing = true

			sut = tr.configGivenEnv()

			tr.createContainer(ctx, sut, false)
			tr.startContainer()

			Eventually(hasTraces, 100*time.Second, time.Second).Should(BeTrue())
		}, SpecTimeout(3*time.Minute))

		AfterEach(func() {
			cmd := exec.Command(contribTracingPath + "stop")
			cmd.Stdout = os.Stdout
			Expect(cmd.Run()).To(Succeed())
		})
	})
})
