package client_test

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"time"
)

// busyboxDest is the busybox binary of the specs, BUSYBOX_BINARY overrides it.
var busyboxDest = filepath.Join(busyboxDestDir, "busybox")

var (
	errBusyboxMissing   = errors.New("BUSYBOX_BINARY does not exist")
	errDownloadStatus   = errors.New("unexpected download status")
	errChecksumMismatch = errors.New("checksum mismatch")
)

// cacheBusyBox downloads busybox once per user. A provided BUSYBOX_BINARY
// is used as it is, a cached file is checked and replaced if it does not
// match, an earlier failed download must not stay in use.
func cacheBusyBox() error {
	if os.Getenv("BUSYBOX_BINARY") != "" {
		if _, err := os.Stat(busyboxDest); err != nil {
			return fmt.Errorf("%w: %s", errBusyboxMissing, busyboxDest)
		}

		return nil
	}

	if sum, err := fileSHA256(busyboxDest); err == nil && sum == busyboxSHA256 {
		return nil
	}

	if err := os.MkdirAll(busyboxDestDir, 0o755); err != nil {
		return fmt.Errorf("create busybox dest dir: %w", err)
	}

	if err := downloadFile(busyboxSource, busyboxDest, busyboxSHA256); err != nil {
		return fmt.Errorf("download busybox: %w", err)
	}

	return nil
}

// fileSHA256 returns the hex checksum of a file.
func fileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open file: %w", err)
	}
	defer f.Close()

	hash := sha256.New()
	if _, err := io.Copy(hash, f); err != nil {
		return "", fmt.Errorf("read file: %w", err)
	}

	return hex.EncodeToString(hash.Sum(nil)), nil
}

// linkOrCopy hard links src to dst and copies it if the link is not
// possible, for example across file systems.
func linkOrCopy(src, dst string) error {
	if err := os.Link(src, dst); err == nil {
		return nil
	}

	in, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open source: %w", err)
	}
	defer in.Close()

	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o755)
	if err != nil {
		return fmt.Errorf("create destination: %w", err)
	}
	defer out.Close()

	if _, err := io.Copy(out, in); err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

// downloadFile downloads a URL into path. The file only appears at path once
// it is complete and matches the checksum, a failed download must not get
// cached.
func downloadFile(url, path, sha256sum string) error {
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()

	c := http.Client{Timeout: time.Minute}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, http.NoBody)
	if err != nil {
		return fmt.Errorf("create request: %w", err)
	}

	resp, err := c.Do(req)
	if err != nil {
		return fmt.Errorf("get URL: %w", err)
	}

	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%w: %s", errDownloadStatus, resp.Status)
	}

	out, err := os.CreateTemp(filepath.Dir(path), "download-")
	if err != nil {
		return fmt.Errorf("create temp file: %w", err)
	}

	defer os.Remove(out.Name())
	defer out.Close()

	hash := sha256.New()
	if _, err := io.Copy(io.MultiWriter(out, hash), resp.Body); err != nil {
		return fmt.Errorf("copy response: %w", err)
	}

	if sum := hex.EncodeToString(hash.Sum(nil)); sum != sha256sum {
		return fmt.Errorf("%w: %s is %s, expected %s", errChecksumMismatch, url, sum, sha256sum)
	}

	if err := out.Chmod(0o755); err != nil {
		return fmt.Errorf("change permissions: %w", err)
	}

	if err := out.Close(); err != nil {
		return fmt.Errorf("close file: %w", err)
	}

	if err := os.Rename(out.Name(), path); err != nil {
		return fmt.Errorf("move file into place: %w", err)
	}

	return nil
}
