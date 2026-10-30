package client

import (
	"errors"
	"fmt"
	"maps"
	"slices"
	"strings"
	"testing"

	"capnproto.org/go/capnp/v3"
	"github.com/containers/conmon-rs/internal/proto"
	"go.podman.io/storage/pkg/idtools"
)

func TestNamespaceConversion(t *testing.T) {
	t.Parallel()

	for _, namespace := range []Namespace{NamespaceIPC, NamespacePID, NamespaceNet, NamespaceUser, NamespaceUTS} {
		typ, err := namespaceTypes.toProto(namespace)
		if err != nil {
			t.Fatalf("convert namespace %d: %v", namespace, err)
		}

		back, err := namespaceTypes.fromProto(typ)
		if err != nil {
			t.Fatalf("convert namespace %v back: %v", typ, err)
		}

		if back != namespace {
			t.Errorf("namespace %d converted back to %d", namespace, back)
		}
	}

	if _, err := namespaceTypes.toProto(Namespace(42)); !errors.Is(err, ErrInvalidValue) {
		t.Errorf("unknown namespace: got %v, want %v", err, ErrInvalidValue)
	}

	if _, err := namespaceTypes.fromProto(
		proto.Conmon_Namespace(42),
	); !errors.Is(
		err,
		ErrInvalidValue,
	) {
		t.Errorf("unknown namespace from server: got %v, want %v", err, ErrInvalidValue)
	}
}

func TestNamespacesToProto(t *testing.T) {
	t.Parallel()

	mappings := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

	for _, tc := range []struct {
		name string
		cfg  *CreateNamespacesConfig
		err  error
	}{
		{"none", &CreateNamespacesConfig{}, nil},
		{"user with mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceNet, NamespaceUser},
			IDMappings: idtools.NewIDMappingsFromMaps(mappings, mappings),
		}, nil},
		{"user without mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceUser},
		}, ErrMissingIDMappings},
		{"user without GID mappings", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceUser},
			IDMappings: idtools.NewIDMappingsFromMaps(mappings, nil),
		}, ErrMissingIDMappings},
		{"unknown", &CreateNamespacesConfig{
			Namespaces: []Namespace{NamespaceIPC, Namespace(-1)},
		}, ErrInvalidValue},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			namespaces, err := namespacesToProto(tc.cfg)
			if !errors.Is(err, tc.err) {
				t.Fatalf("got error %v, want %v", err, tc.err)
			}

			if err == nil && len(namespaces) != len(tc.cfg.Namespaces) {
				t.Errorf("got %d namespaces, want %d", len(namespaces), len(tc.cfg.Namespaces))
			}
		})
	}
}

func TestMappingsToSlice(t *testing.T) {
	t.Parallel()

	// The server parses "<container ID> <host ID> <size>" per mapping.
	got := mappingsToSlice([]idtools.IDMap{
		{ContainerID: 0, HostID: 100000, Size: 65536},
		{ContainerID: 65536, HostID: 1000, Size: 1},
	})
	if want := []string{"0 100000 65536", "65536 1000 1"}; !slices.Equal(got, want) {
		t.Fatalf("got mappings %q, want %q", got, want)
	}

	if got := mappingsToSlice(nil); len(got) != 0 {
		t.Fatalf("got mappings %q without any", got)
	}
}

// namespacesResponse returns a response of the server with the namespaces,
// ordered by their values.
func namespacesResponse(
	t *testing.T, namespaces map[proto.Conmon_Namespace]string,
) proto.Conmon_CreateNamespacesResponse {
	t.Helper()

	_, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	response, err := proto.NewRootConmon_CreateNamespacesResponse(seg)
	if err != nil {
		t.Fatalf("new response: %v", err)
	}

	list, err := response.NewNamespaces(int32(len(namespaces)))
	if err != nil {
		t.Fatalf("new namespaces: %v", err)
	}

	for i, typ := range slices.Sorted(maps.Keys(namespaces)) {
		list.At(i).SetType(typ)

		if err := list.At(i).SetPath(namespaces[typ]); err != nil {
			t.Fatalf("set path: %v", err)
		}
	}

	return response
}

func TestReadCreateNamespacesResponse(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name       string
		namespaces map[proto.Conmon_Namespace]string
		want       []*NamespacesResponse
		wantErr    error
	}{
		{
			name: "none",
			want: []*NamespacesResponse{},
		},
		{
			name: "all",
			namespaces: map[proto.Conmon_Namespace]string{
				proto.Conmon_Namespace_ipc:  "/run/ipcns/id",
				proto.Conmon_Namespace_pid:  "/run/pidns/id",
				proto.Conmon_Namespace_net:  "/run/netns/id",
				proto.Conmon_Namespace_user: "/run/userns/id",
				proto.Conmon_Namespace_uts:  "/run/utsns/id",
			},
			want: []*NamespacesResponse{
				// In the order of the values of the server, which differ
				// from the ones of the client.
				{Type: NamespaceIPC, Path: "/run/ipcns/id"},
				{Type: NamespaceNet, Path: "/run/netns/id"},
				{Type: NamespacePID, Path: "/run/pidns/id"},
				{Type: NamespaceUser, Path: "/run/userns/id"},
				{Type: NamespaceUTS, Path: "/run/utsns/id"},
			},
		},
		{
			// From a newer server.
			name: "unknown namespace",
			namespaces: map[proto.Conmon_Namespace]string{
				proto.Conmon_Namespace_net: "/run/netns/id",
				proto.Conmon_Namespace(42): "/run/newns/id",
			},
			wantErr: ErrInvalidValue,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got, err := readCreateNamespacesResponse(namespacesResponse(t, tc.namespaces))
			if tc.wantErr != nil {
				if !errors.Is(err, tc.wantErr) {
					t.Fatalf("got error %v, want %v", err, tc.wantErr)
				}

				return
			}

			if err != nil {
				t.Fatalf("read response: %v", err)
			}

			if !slices.EqualFunc(got.Namespaces, tc.want, func(a, b *NamespacesResponse) bool {
				return *a == *b
			}) {
				t.Fatalf(
					"got namespaces %s, want %s",
					formatNamespaces(got.Namespaces),
					formatNamespaces(tc.want),
				)
			}
		})
	}
}

// formatNamespaces formats the namespaces of a response for a failure.
func formatNamespaces(namespaces []*NamespacesResponse) string {
	formatted := make([]string, 0, len(namespaces))
	for _, namespace := range namespaces {
		formatted = append(formatted, fmt.Sprintf("%+v", *namespace))
	}

	return strings.Join(formatted, ", ")
}
