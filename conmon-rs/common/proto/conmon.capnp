@0xffaaf7385bc4adad;

interface Conmon {
    struct VersionResponse {
        version @0 :Text;
    }

    version @0 () -> (response: VersionResponse);
}
