package client_test

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
)

// serversInRunDirs finds servers launched for these runtime directories,
// including failed launches which never returned a client. Exact argv entries
// avoid matching unrelated processes whose arguments only mention a binary
// name or contain a shell command. Zombies have an empty cmdline and are done.
func serversInRunDirs(
	procRoot string, dirs map[string]struct{}, readCmdline func(string) ([]byte, error),
) ([]string, error) {
	entries, err := os.ReadDir(procRoot)
	if err != nil {
		return nil, fmt.Errorf("read process directory: %w", err)
	}

	var servers []string

	for _, entry := range entries {
		if _, err := strconv.ParseUint(entry.Name(), 10, 32); err != nil {
			continue
		}

		cmdline, err := readCmdline(filepath.Join(procRoot, entry.Name(), "cmdline"))
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.ESRCH) ||
			errors.Is(err, fs.ErrPermission) {
			// ESRCH also occurs if it exits after opening cmdline,
			// before reading it. Other users may deny access.
			continue
		}

		if err != nil {
			return nil, fmt.Errorf("read command line of PID %s: %w", entry.Name(), err)
		}

		if dir := runtimeDirArg(strings.Split(string(cmdline), "\x00")); dir != "" {
			if _, owned := dirs[dir]; owned {
				servers = append(servers, fmt.Sprintf("PID %s in %s", entry.Name(), dir))
			}
		}
	}

	return servers, nil
}

func runtimeDirArg(args []string) string {
	// Skip argv[0], which is the executable, not an option.
	for i := 1; i < len(args); i++ {
		if args[i] == "--runtime-dir" && i+1 < len(args) {
			return args[i+1]
		}

		if dir, ok := strings.CutPrefix(args[i], "--runtime-dir="); ok {
			return dir
		}
	}

	return ""
}

func TestServersInRunDirs(t *testing.T) {
	t.Parallel()

	proc := t.TempDir()
	dir := "/tmp/suite with spaces"

	for pid, args := range map[string][]string{
		"1": {"conmonrs", "--runtime-dir", dir},
		"2": {"conmonrs", "--runtime-dir=" + dir},
		"3": {"conmonrs", "--runtime-dir", dir + "-other"},
		"4": {"sh", "-c", "conmonrs --runtime-dir " + dir},
		"5": {"conmonrs", "--runtime-dir"},
		"6": {}, // A zombie.
	} {
		path := filepath.Join(proc, pid)
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}

		if err := os.WriteFile(
			filepath.Join(path, "cmdline"),
			[]byte(strings.Join(args, "\x00")),
			0o600,
		); err != nil {
			t.Fatal(err)
		}
	}

	// A process which exited between listing and reading, and a non-PID.
	for _, name := range []string{"7", "8", "self"} {
		if err := os.Mkdir(filepath.Join(proc, name), 0o700); err != nil {
			t.Fatal(err)
		}
	}

	readCmdline := func(path string) ([]byte, error) {
		if path == filepath.Join(proc, "8", "cmdline") {
			// A process exiting after open makes procfs return ESRCH from
			// read, rather than the ENOENT of the missing PID 7 above.
			return nil, &os.PathError{Op: "read", Path: path, Err: syscall.ESRCH}
		}

		return os.ReadFile(path)
	}

	servers, err := serversInRunDirs(proc, map[string]struct{}{dir: {}}, readCmdline)
	if err != nil {
		t.Fatal(err)
	}

	if got, want := strings.Join(servers, ", "), "PID 1 in "+dir+", PID 2 in "+dir; got != want {
		t.Fatalf("servers = %q, want %q", got, want)
	}
}
