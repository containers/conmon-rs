package client_test

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
)

// Starts or connects to the server of a run directory and releases the
// client again, the server keeps running.
func ExampleNew() {
	cfg := client.NewConmonServerConfig("/usr/bin/crun", "", "/run/conmonrs/pod")

	conmon, err := client.New(cfg)
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	fmt.Println("server PID:", conmon.PID())
}

// Stops the server, with SIGKILL if it does not exit on its own.
func ExampleConmonClient_ShutdownContext() {
	conmon, err := client.New(
		client.NewConmonServerConfig("/usr/bin/crun", "", "/run/conmonrs/pod"),
	)
	if err != nil {
		fmt.Println("start server:", err)

		return
	}

	ctx := context.Background()

	err = conmon.ShutdownContext(ctx)
	if errors.Is(err, client.ErrShutdownTimeout) {
		err = conmon.KillContext(ctx)
	}

	if err != nil {
		fmt.Println("stop server:", err)
	}
}

// Asks the server whether it supports a feature before using it.
func ExampleConmonClient_Version() {
	conmon, err := client.New(
		client.NewConmonServerConfig("/usr/bin/crun", "", "/run/conmonrs/pod"),
	)
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	version, err := conmon.Version(context.Background(), &client.VersionConfig{})
	if err != nil {
		fmt.Println("get version:", err)

		return
	}

	if slices.Contains(version.Features, client.FeatureIdleTimeout) {
		fmt.Println("idle timeouts apply")
	}
}

// Runs a command in a container, bounded by a timeout.
func ExampleConmonClient_ExecSyncContainer() {
	conmon, err := client.New(
		client.NewConmonServerConfig("/usr/bin/crun", "", "/run/conmonrs/pod"),
	)
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	result, err := conmon.ExecSyncContainer(context.Background(), &client.ExecSyncConfig{
		ID:      "container",
		Command: []string{"/bin/true"},
		Timeout: 10,
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
	conmon, err := client.New(
		client.NewConmonServerConfig("/usr/bin/crun", "", "/run/conmonrs/pod"),
	)
	if err != nil {
		fmt.Println("start server:", err)

		return
	}
	defer conmon.Close()

	result, err := conmon.ServeExecContainer(context.Background(), &client.ServeExecContainerConfig{
		ID:          "container",
		Command:     []string{"/bin/sh"},
		Tty:         true,
		Stdin:       true,
		Stdout:      true,
		IdleTimeout: 4 * time.Hour,
	})
	if err != nil {
		fmt.Println("serve exec:", err)

		return
	}

	fmt.Println("connect to:", result.URL)
}
