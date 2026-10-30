package client

import (
	"context"
	"errors"
	"slices"
	"testing"
	"time"

	"capnproto.org/go/capnp/v3"
	"github.com/containers/conmon-rs/internal/proto"
	"github.com/sirupsen/logrus"
)

func TestLogDriverTypes(t *testing.T) {
	t.Parallel()

	for typ, want := range map[LogDriverType]proto.Conmon_LogDriver_Type{
		LogDriverTypeContainerRuntimeInterface: proto.Conmon_LogDriver_Type_containerRuntimeInterface,
		LogDriverTypeJSONLogger:                proto.Conmon_LogDriver_Type_json,
		LogDriverTypeJournald:                  proto.Conmon_LogDriver_Type_journald,
	} {
		got, err := logDriverTypes.toProto(typ)
		if err != nil || got != want {
			t.Errorf("type %d: got %v, %v, want %v", typ, got, err, want)
		}
	}

	labels := map[string]string{"POD": "web"}
	logDrivers := []ContainerLogDriver{
		{Type: LogDriverTypeJournald, ContainerName: "name", Tag: "tag", Labels: labels},
		{Type: LogDriverTypeJSONLogger, Path: "log", MaxSize: 1},
	}

	types, err := validateLogDrivers(logDrivers)
	if err != nil {
		t.Fatalf("validate log drivers: %v", err)
	}

	// The request carries the fields of the journald driver.
	_, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	req, err := proto.NewRootConmon_CreateContainerRequest(seg)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}

	err = initLogDrivers(&req, logDrivers, types)
	if err != nil {
		t.Fatalf("init log drivers: %v", err)
	}

	list, err := req.LogDrivers()
	if err != nil {
		t.Fatalf("log drivers: %v", err)
	}

	if list.Len() != 2 || list.At(0).Type() != proto.Conmon_LogDriver_Type_journald {
		t.Fatalf("got %d log drivers, the first of type %v", list.Len(), list.At(0).Type())
	}

	json := list.At(1)

	path, err := json.Path()
	if err != nil || json.Type() != proto.Conmon_LogDriver_Type_json ||
		path != "log" || json.MaxSize() != 1 {
		t.Errorf("got JSON driver %v, %q, %d, %v", json.Type(), path, json.MaxSize(), err)
	}

	journald := list.At(0)

	entries, err := journald.Labels()
	if err != nil || entries.Len() != 1 {
		t.Fatalf("labels: %v, %d entries", err, entries.Len())
	}

	texts := []func() (string, error){
		journald.ContainerName, journald.Tag, entries.At(0).Key, entries.At(0).Value,
	}
	got := make([]string, 0, len(texts))

	for _, text := range texts {
		value, textErr := text()
		if textErr != nil {
			t.Fatalf("read text: %v", textErr)
		}

		got = append(got, value)
	}

	if want := []string{"name", "tag", "POD", "web"}; !slices.Equal(got, want) {
		t.Errorf("got name, tag and label %q, want %q", got, want)
	}

	if _, err := validateLogDrivers([]ContainerLogDriver{
		{Type: LogDriverTypeJournald}, {Type: LogDriverType(42)},
	}); !errors.Is(err, ErrInvalidValue) {
		t.Errorf("unknown type: got %v, want %v", err, ErrInvalidValue)
	}
}

func TestCreateContainerValidatesWithoutServer(t *testing.T) {
	t.Parallel()

	// No server runs, the configuration fails before the feature check
	// of the sd_notify socket dials one.
	c := newTestClient(t, t.TempDir(), logrus.New())

	if _, err := c.CreateContainer(t.Context(), &CreateContainerConfig{
		ID:             "id",
		LogDrivers:     []ContainerLogDriver{{Type: LogDriverType(42)}},
		SdNotifySocket: "/notify.sock",
	}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("got error %v, want %v", err, ErrInvalidValue)
	}
}

func TestExecTimeout(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name     string
		deadline time.Duration // none if 0
		timeout  uint64
		want     uint64
	}{
		{"explicit timeout below deadline", time.Hour, 5, 5},
		{"explicit timeout limited by deadline", 10500 * time.Millisecond, 3600, 9},
		{"explicit timeout a second below deadline", 10500 * time.Millisecond, 9, 9},
		{"explicit timeout at deadline", 10500 * time.Millisecond, 10, 9},
		{"explicit timeout without deadline", 0, 7, 7},
		{"no deadline", 0, 0, 0},
		{"deadline rounded down minus a second", 10500 * time.Millisecond, 0, 9},
		{"two seconds left", 2500 * time.Millisecond, 0, 1},
		{"less than two seconds left", 1900 * time.Millisecond, 0, 1},
		{"short deadline", 500 * time.Millisecond, 0, 1},
		{"expired deadline", -time.Second, 0, 1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			ctx := context.Background()

			if tc.deadline != 0 {
				var cancel context.CancelFunc

				ctx, cancel = context.WithTimeout(ctx, tc.deadline)
				defer cancel()
			}

			if got := execTimeout(ctx, tc.timeout); got != tc.want {
				t.Errorf("got %d, want %d", got, tc.want)
			}
		})
	}
}
