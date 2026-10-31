#include <dave/dave_interfaces.h>
#include <memory>

// This bridge uses the shipped libdave C++ interface. The pinned C API handle
// is ISession* (bindings_capi.cpp). Keep all other calls on the stable C API.
struct Identity {
    std::shared_ptr<mlspp::SignaturePrivateKey> key;
};

extern "C" void* suncordDaveIdentityCreate() noexcept {
    try {
        return new Identity;
    } catch (...) {
        return nullptr;
    }
}

extern "C" void suncordDaveIdentityDestroy(void* identity) noexcept {
    delete static_cast<Identity*>(identity);
}

extern "C" bool suncordDaveSessionInit(void* session, void* identity,
                                     uint16_t version, uint64_t group,
                                     const char* user) noexcept {
    if (!session || !identity || !user) return false;
    try {
        auto& shared = *static_cast<Identity*>(identity);
        reinterpret_cast<discord::dave::mls::ISession*>(session)->Init(
            version, group, std::string(user), shared.key);
        return bool(shared.key);
    } catch (...) {
        return false;
    }
}
