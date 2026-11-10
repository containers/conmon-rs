package client_test

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
)

// The examples have no "// Output:" comments on purpose: they need a server
// to run against, so go test only compiles them.

// Starts or connects to the server of a run directory and releases the
// client again, the server keeps running.
func ExampleNew() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
		LogDriver:    client.LogDriverSystemd,
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	fmt.Println("server PID:", conmon.PID())
}

// Stops the server, with SIGKILL if it does not exit on its own.
func ExampleConmonClient_Shutdown() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}

	ctx := context.Background()

	err = conmon.Shutdown(ctx)
	if errors.Is(err, client.ErrShutdownTimeout) {
		err = conmon.Kill(ctx)
	}

	if err != nil {
		fmt.Println("stop server:", err)
	}
}

// Asks the server for its version, like for a log line. Check the features
// with SupportsFeature, see its example.
func ExampleConmonClient_Version() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	version, err := conmon.Version(context.Background())
	if err != nil {
		fmt.Println("get version:", err)

		return
	}

	fmt.Printf("conmon-rs %s (commit %s), PID %d\n",
		version.Version, version.Commit, version.ProcessID)
}

// Checks a feature of the running server, which may be older than the
// binary on disk.
func ExampleConmonClient_SupportsFeature() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	if conmon.SupportsFeature(client.FeatureStreamingProtocols) {
		fmt.Println("exec and attach speak SPDY/3.1 as well")
	} else {
		fmt.Println("exec and attach only speak v5.channel.k8s.io")
	}
}

// Runs a command in a container, bounded by a timeout.
func ExampleConmonClient_ExecSyncContainer() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	result, err := conmon.ExecSyncContainer(context.Background(), &client.ExecSyncContainerConfig{
		ID:      "container",
		Command: []string{"/bin/true"},
		Timeout: 10 * time.Second,
	})

	switch {
	case err != nil:
		fmt.Println("exec:", err)
	case result.ExitCode == client.ExecExitCodeNotStarted:
		// Also for a container the runtime does not know.
		fmt.Println("not started:", string(result.Stderr))
	case result.TimedOut:
		fmt.Println("timed out")
	default:
		fmt.Println("exit code:", result.ExitCode)
	}
}

// Hands out a URL for an exec session, which a streaming client connects to.
func ExampleConmonClient_ServeExecContainer() {
	conmon, err := client.New(&client.ConmonServerConfig{
		Runtime:      "/usr/bin/crun",
		ServerRunDir: "/run/conmonrs/pod",
	})
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	result, err := conmon.ServeExecContainer(context.Background(), &client.ServeExecContainerConfig{
		ID:          "container",
		Command:     []string{"/bin/sh"},
		Terminal:    true,
		Stdin:       true,
		Stdout:      true,
		IdleTimeout: 4 * time.Hour,
	})
	if errors.Is(err, client.ErrTooManyRequests) {
		// Too many URLs wait for their client, a later request may succeed.
		fmt.Println("try again later:", err)

		return
	}

	if err != nil {
		fmt.Println("serve exec:", err)

		return
	}

	fmt.Println("connect to:", result.URL)
}
