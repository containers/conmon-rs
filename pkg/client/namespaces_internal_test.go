package client

import (
	"errors"
	"slices"
	"testing"

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

	if _, err := namespaceTypes.toProto(Namespace(42)); !errors.Is(err, errInvalidValue) {
		t.Errorf("unknown namespace: got %v, want %v", err, errInvalidValue)
	}

	if _, err := namespaceTypes.fromProto(
		proto.Conmon_Namespace(42),
	); !errors.Is(
		err,
		errInvalidValue,
	) {
		t.Errorf("unknown namespace from server: got %v, want %v", err, errInvalidValue)
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
		}, errInvalidValue},
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
