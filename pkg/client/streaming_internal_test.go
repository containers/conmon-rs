package client

import (
	"math"
	"strings"
	"testing"
	"time"

	"github.com/sirupsen/logrus"
	logrustest "github.com/sirupsen/logrus/hooks/test"
)

func TestTimeoutSeconds(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		timeout time.Duration
		want    uint64
	}{
		{timeout: -time.Second, want: 0},
		{timeout: 0, want: 0},
		{timeout: time.Nanosecond, want: 1},
		{timeout: time.Second, want: 1},
		{timeout: time.Second + 1, want: 2},
		{timeout: 1500 * time.Millisecond, want: 2},
		{timeout: math.MinInt64, want: 0},
		// Within a second of the largest duration, where adding a second
		// for the rounding would overflow.
		{timeout: math.MaxInt64, want: math.MaxInt64/uint64(time.Second) + 1},
	} {
		if got := timeoutSeconds(tc.timeout); got != tc.want {
			t.Errorf("timeoutSeconds(%d) = %d, want %d", tc.timeout, got, tc.want)
		}
	}
}

func TestIgnoredIdleTimeoutWarnsOnce(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name     string
		features []string
		// unknown leaves the features of the client unknown, like those of
		// a client which did not ask the server yet.
		unknown bool
		timeout time.Duration
		warned  bool
	}{
		{name: "without the feature", timeout: time.Second, warned: true},
		{name: "with the feature", features: []string{FeatureIdleTimeout}, timeout: time.Second},
		{name: "without a timeout"},
		{name: "unknown features", unknown: true, timeout: time.Second},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			requests, _ := serveRecording(t, dir, tc.features...)

			logger, hook := logrustest.NewNullLogger()
			c := newTestClient(t, dir, logger)

			t.Cleanup(func() { c.Close() })

			if !tc.unknown {
				// A new connection only refreshes the features once some
				// are known. Known but empty, the call then records those
				// of the fake server when it connects.
				c.setFeatures([]string{})
			}

			for range 2 {
				if _, err := c.ServeExecContainer(t.Context(), &ServeExecContainerConfig{
					ID:          "id",
					IdleTimeout: tc.timeout,
				}); err != nil {
					t.Fatalf("serve exec: %v", err)
				}

				// The server blocks once its channel is full.
				lastRequest(t, requests)

				if _, err := c.ServeAttachContainer(t.Context(), &ServeAttachContainerConfig{
					ID:          "id",
					IdleTimeout: tc.timeout,
				}); err != nil {
					t.Fatalf("serve attach: %v", err)
				}

				lastRequest(t, requests)
			}

			warnings := 0

			for _, entry := range hook.AllEntries() {
				if entry.Level == logrus.WarnLevel &&
					strings.Contains(entry.Message, "ignores the idle timeout") {
					warnings++
				}
			}

			want := 0
			if tc.warned {
				want = 1
			}

			if warnings != want {
				t.Fatalf("got %d warnings, want %d: %v", warnings, want, hook.AllEntries())
			}
		})
	}
}
