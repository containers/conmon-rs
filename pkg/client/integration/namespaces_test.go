package client_test

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/containers/conmon-rs/pkg/client"
	"github.com/google/uuid"
	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
	"go.podman.io/storage/pkg/idtools"
	"go.podman.io/storage/pkg/unshare"
)

var _ = Describe("ConmonClient", func() {
	var (
		tr  *testRunner
		sut *client.ConmonClient
	)

	registerSpecHooks(&tr, &sut)

	Describe("CreateNamespaces", func() {
		// The namespaces get bind mounted, which needs privileges.
		BeforeEach(func() {
			if unshare.IsRootless() {
				Skip("does not run rootless")
			}
		})

		It("should succeed without namespaces", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					PodID: podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())
		}, SpecTimeout(specTimeout))

		It("should fail without pod ID", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{},
			)
			Expect(err).To(MatchError(ContainSubstring("no pod ID provided")))
			Expect(response).To(BeNil())
		}, SpecTimeout(specTimeout))

		It("should succeed without user namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					PodID: podID,
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespacePID,
						client.NamespaceUTS,
					},
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())

			// Only the requested namespaces are created, in request order.
			Expect(response.Namespaces).To(HaveLen(4))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUTS))

			const basePath = "/var/run/"

			for i, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
				Expect(filepath.Base(ns.Path)).To(Equal(podID))

				switch i {
				case 0:
					Expect(ns.Path).To(ContainSubstring(basePath + "ipcns/"))
				case 1:
					Expect(ns.Path).To(ContainSubstring(basePath + "netns/"))
				case 2:
					Expect(ns.Path).To(ContainSubstring(basePath + "pidns/"))
				case 3:
					Expect(ns.Path).To(ContainSubstring(basePath + "utsns/"))
				}
			}

			// The user namespace was not requested and is not bound.
			Expect(filepath.Join(basePath, "userns", podID)).NotTo(BeAnExistingFile())
		}, SpecTimeout(specTimeout))

		It("should not keep a pause process without PID namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespaceUTS,
					},
					BasePath: basePath,
					PodID:    podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())
			Expect(response.Namespaces).To(HaveLen(3))

			// The bind mounts keep the namespaces alive on their own.
			for _, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
			}

			// Without a PID namespace no pause process stays behind.
			Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).NotTo(BeAnExistingFile())
			Expect(pauseProcessRunning(podID)).To(BeFalse())
		}, SpecTimeout(specTimeout))

		It(
			"should not keep a pause process with user namespace and without PID namespace",
			func(ctx SpecContext) {
				tr = newTestRunner()
				tr.createRuntimeConfig(false)
				sut = tr.configGivenEnv()

				basePath := MustTempDir("ns-test-")
				// Runs after the server shutdown unmounted the namespaces.
				DeferCleanup(os.RemoveAll, basePath)

				podID := uuid.New().String()

				uids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}
				gids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

				response, err := sut.CreateNamespaces(
					ctx,
					&client.CreateNamespacesConfig{
						Namespaces: []client.Namespace{
							client.NamespaceIPC,
							client.NamespaceNet,
							client.NamespaceUTS,
							client.NamespaceUser,
							// Duplicates are reported once.
							client.NamespaceNet,
						},
						IDMappings: idtools.NewIDMappingsFromMaps(uids, gids),
						BasePath:   basePath,
						PodID:      podID,
					},
				)
				Expect(err).To(Succeed())
				Expect(response).NotTo(BeNil())
				Expect(response.Namespaces).To(HaveLen(4))
				Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUser))

				// The namespaces are bound on the host and outlive the pause process.
				for _, ns := range response.Namespaces {
					expectNamespaceFile(ns.Path)
				}

				Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).NotTo(BeAnExistingFile())
				Expect(pauseProcessRunning(podID)).To(BeFalse())
			},
			SpecTimeout(specTimeout),
		)

		It("should succeed with user namespace and custom base path", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			uids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}
			gids := []idtools.IDMap{{ContainerID: 0, HostID: 0, Size: 1}}

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespacePID,
						client.NamespaceUTS,
						client.NamespaceUser,
					},
					IDMappings: idtools.NewIDMappingsFromMaps(uids, gids),
					BasePath:   basePath,
					PodID:      podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response).NotTo(BeNil())

			Expect(response.Namespaces).To(HaveLen(5))
			Expect(response.Namespaces[0].Type).To(Equal(client.NamespaceIPC))
			Expect(response.Namespaces[1].Type).To(Equal(client.NamespaceNet))
			Expect(response.Namespaces[2].Type).To(Equal(client.NamespacePID))
			Expect(response.Namespaces[3].Type).To(Equal(client.NamespaceUTS))
			Expect(response.Namespaces[4].Type).To(Equal(client.NamespaceUser))

			for _, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)
			}

			// The PID namespace needs the pause process to stay alive.
			Expect(filepath.Join(basePath, "conmonrs", podID+".pid")).To(BeAnExistingFile())
			Expect(pauseProcessRunning(podID)).To(BeTrue())
		}, SpecTimeout(specTimeout))

		It("should unmount namespaces which are held open", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			DeferCleanup(os.RemoveAll, basePath)

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceIPC,
						client.NamespaceNet,
						client.NamespacePID,
						client.NamespaceUTS,
					},
					BasePath: basePath,
					PodID:    uuid.New().String(),
				},
			)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(4))

			// The test holds the bind paths open, which makes a plain
			// unmount fail with EBUSY, like a process which keeps a
			// namespace file open.
			for _, ns := range response.Namespaces {
				expectNamespaceFile(ns.Path)

				file, err := os.Open(ns.Path)
				Expect(err).To(Succeed())
				DeferCleanup(file.Close)
			}

			DeferCleanup(sut.Close)
			Expect(sut.Shutdown()).To(Succeed())

			sut = nil

			for _, ns := range response.Namespaces {
				Expect(ns.Path).NotTo(BeAnExistingFile())
			}
		}, SpecTimeout(specTimeout))

		It("should reap orphans in the pod PID namespace", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{client.NamespacePID},
					BasePath:   basePath,
					PodID:      podID,
				},
			)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(1))

			pidFile, err := os.ReadFile(filepath.Join(basePath, "conmonrs", podID+".pid"))
			Expect(err).To(Succeed())
			pausePID, err := strconv.Atoi(strings.TrimSpace(string(pidFile)))
			Expect(err).To(Succeed())

			// The shell exits right away, so its background child gets
			// reparented to the pause process, the init of the PID namespace.
			//nolint:gosec // test helper with controlled inputs
			cmd := exec.CommandContext(
				ctx,
				"nsenter", "--pid="+response.Namespaces[0].Path, "--",
				"sh", "-c", "sleep 1 & exit 0",
			)
			Expect(cmd.Run()).To(Succeed())
			Expect(childrenOf(pausePID)).NotTo(BeEmpty())

			// Once the orphan exited, it does not stay a zombie.
			Eventually(func() []int { return childrenOf(pausePID) }).
				WithTimeout(10 * time.Second).
				Should(BeEmpty())
		}, SpecTimeout(specTimeout))

		It("should fail for another pod or configuration", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			cfg := client.CreateNamespacesConfig{
				Namespaces: []client.Namespace{client.NamespaceIPC, client.NamespaceUTS},
				BasePath:   basePath,
				PodID:      uuid.New().String(),
			}
			response, err := sut.CreateNamespaces(ctx, &cfg)
			Expect(err).To(Succeed())
			Expect(response.Namespaces).To(HaveLen(2))

			// The same request is answered with the same namespaces, in any order.
			same := cfg
			same.Namespaces = []client.Namespace{client.NamespaceUTS, client.NamespaceIPC}
			again, err := sut.CreateNamespaces(ctx, &same)
			Expect(err).To(Succeed())
			Expect(again.Namespaces).To(Equal(response.Namespaces))

			otherPod := cfg
			otherPod.PodID = uuid.New().String()
			_, err = sut.CreateNamespaces(ctx, &otherPod)
			Expect(err).To(MatchError(ContainSubstring("cannot serve pod " + otherPod.PodID)))

			otherNamespaces := cfg
			otherNamespaces.Namespaces = []client.Namespace{client.NamespaceNet}
			_, err = sut.CreateNamespaces(ctx, &otherNamespaces)
			Expect(err).To(MatchError(ContainSubstring("created with namespaces")))

			otherBasePath := cfg
			otherBasePath.BasePath = MustTempDir("ns-test-")
			DeferCleanup(os.RemoveAll, otherBasePath.BasePath)
			_, err = sut.CreateNamespaces(ctx, &otherBasePath)
			Expect(err).To(MatchError(ContainSubstring("created with base path")))
		}, SpecTimeout(specTimeout))

		It("should create the namespaces once for concurrent calls", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			basePath := MustTempDir("ns-test-")
			// Runs after the server shutdown unmounted the namespaces.
			DeferCleanup(os.RemoveAll, basePath)

			podID := uuid.New().String()
			cfg := &client.CreateNamespacesConfig{
				Namespaces: []client.Namespace{
					client.NamespaceIPC,
					client.NamespacePID,
					client.NamespaceUTS,
				},
				BasePath: basePath,
				PodID:    podID,
			}

			const calls = 10

			responses := make([]*client.CreateNamespacesResponse, calls)

			var wg sync.WaitGroup
			for i := range calls {
				wg.Go(func() {
					defer GinkgoRecover()

					response, err := sut.CreateNamespaces(ctx, cfg)
					Expect(err).To(Succeed())

					responses[i] = response
				})
			}

			wg.Wait()

			for _, response := range responses {
				Expect(response.Namespaces).To(Equal(responses[0].Namespaces))
			}

			for _, ns := range responses[0].Namespaces {
				expectNamespaceFile(ns.Path)
			}

			Expect(pauseProcessCount(podID)).To(Equal(1))
		}, SpecTimeout(specTimeout))

		It("should fail with user namespace without mappings", func(ctx SpecContext) {
			tr = newTestRunner()
			tr.createRuntimeConfig(false)
			sut = tr.configGivenEnv()

			response, err := sut.CreateNamespaces(
				ctx,
				&client.CreateNamespacesConfig{
					Namespaces: []client.Namespace{
						client.NamespaceUser,
					},
				},
			)
			Expect(err).NotTo(Succeed())
			Expect(errors.Is(err, client.ErrMissingIDMappings)).To(BeTrue())
			Expect(response).To(BeNil())
		}, SpecTimeout(specTimeout))
	})
})
