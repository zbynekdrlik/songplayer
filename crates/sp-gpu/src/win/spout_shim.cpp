// The `extern "C"` shim between sp-gpu's `SpoutSender` (src/win/spout_sender.rs) and
// the vendored Spout2 SDK 2.007.017 sender, SpoutDX (vendor/spout2) — #223 S1b.
//
// It refuses a second sender under a name another sender holds. Spout itself
// would rename it (`SP-program-MAX_1`), but Resolume Arena binds its layer to
// `SPOUT_SP-program-MAX`, so a renamed sender would feed a layer nobody shows.
//
// Every entry point catches every C++ exception: nothing unwinds into Rust.
// The status codes must match `sp_gpu::spout::status` (src/spout.rs).

#include "SpoutDX.h"

#include <cstring>

namespace {

constexpr int kOk = 0;
constexpr int kNameTaken = 1;
constexpr int kNotRegistered = 2;
constexpr int kFailed = 3;
constexpr int kException = 4;
constexpr int kBadArgument = 5;

// The longest name Spout can carry: it builds `<name>_Count_Semaphore` in 256
// bytes with sprintf_s, which aborts the process on overflow. Must match
// `sp_gpu::SPOUT_NAME_MAX_LEN`.
constexpr size_t kMaxNameLen = 239;

struct Sender {
    spoutDX dx;
    // The name asked for. Spout may register another one (see send).
    char name[256] = {};
    // The first SendTexture registered `name` and Spout lists it.
    bool registered = false;
    // Spout registered another name, or none: this sender never sends again.
    bool refused = false;
};

}  // namespace

extern "C" {

// A sender on `device` (the caller keeps the device alive until release:
// spoutDX does not AddRef it) under `name`, not yet registered: Spout lists
// it at its first send. NULL with `*status` set when the name is taken by a
// listed sender (after Spout's own clean-up of senders that are gone) or on
// any failure.
void* spout_sender_create(ID3D11Device* device, const char* name, int* status) {
    if (status == nullptr) {
        return nullptr;
    }
    if (device == nullptr || name == nullptr || name[0] == '\0' ||
        strnlen(name, kMaxNameLen + 1) > kMaxNameLen) {
        *status = kBadArgument;
        return nullptr;
    }
    Sender* sender = nullptr;
    try {
        sender = new Sender();
        std::memcpy(sender->name, name, std::strlen(name) + 1);
        sender->dx.OpenDirectX11(device);
        // A sender that crashed leaves its name listed with no info map;
        // Spout's own clean-up drops it, so only a live sender blocks us.
        sender->dx.sendernames.CleanSenders();
        if (sender->dx.sendernames.FindSenderName(sender->name)) {
            delete sender;
            *status = kNameTaken;
            return nullptr;
        }
        sender->dx.SetSenderName(sender->name);
        // SetSenderName renames a taken name: another sender took it since
        // the check above.
        if (std::strcmp(sender->dx.GetName(), sender->name) != 0) {
            delete sender;
            *status = kNameTaken;
            return nullptr;
        }
        *status = kOk;
        return sender;
    } catch (...) {
        delete sender;
        *status = kException;
        return nullptr;
    }
}

// Send `texture` (on the sender's device): spoutDX::SendTexture copies it
// into Spout's own shared texture under the sender's named mutex. The first
// send registers the sender; if Spout registered another name (a sender took
// ours in between) or did not list it (its sender list is full), that
// registration is released at once and the sender is refused for good.
int spout_sender_send(void* handle, ID3D11Texture2D* texture) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || texture == nullptr) {
        return kBadArgument;
    }
    if (sender->refused) {
        return kNotRegistered;
    }
    try {
        if (!sender->dx.SendTexture(texture)) {
            return kFailed;
        }
        if (!sender->registered) {
            if (std::strcmp(sender->dx.GetName(), sender->name) != 0 ||
                !sender->dx.sendernames.FindSenderName(sender->name)) {
                sender->dx.ReleaseSender();
                sender->refused = true;
                return kNotRegistered;
            }
            sender->registered = true;
        }
        return kOk;
    } catch (...) {
        return kException;
    }
}

// The size Spout's sender shares (0 x 0 before the first send).
void spout_sender_size(void* handle, unsigned int* width, unsigned int* height) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || width == nullptr || height == nullptr) {
        return;
    }
    *width = sender->dx.GetWidth();
    *height = sender->dx.GetHeight();
}

// Release the sender: ~spoutDX unregisters it (off Spout's list, its info
// map closed) and releases its context reference. NULL is a no-op.
void spout_sender_release(void* handle) {
    delete static_cast<Sender*>(handle);
}

}  // extern "C"
