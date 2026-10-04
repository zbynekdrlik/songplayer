// The `extern "C"` shim between sp-gpu's `SpoutSender`
// (src/win/spout_sender.rs) and the vendored Spout2 SDK 2.007.017 sender,
// SpoutDX (vendor/spout2) — #223 S1b.
//
// It refuses a second sender under a name another sender holds. Spout itself
// would rename it (`SP-program-MAX_1`), but Resolume Arena binds its layer to
// `SPOUT_SP-program-MAX`, so a renamed sender would feed a layer nobody shows.
//
// `create` and `send` catch every C++ exception, `size` cannot throw, and
// `release` runs ~spoutDX (a destructor: a throw there terminates, it never
// unwinds), so nothing unwinds into Rust. The status codes must match
// `sp_gpu::spout::status` (src/spout.rs).

#include "SpoutDX.h"

#include <cstddef>
#include <cstring>
#include <set>
#include <string>

namespace {

constexpr int kOk = 0;
constexpr int kNameTaken = 1;
constexpr int kRenamed = 2;
constexpr int kFailed = 3;
constexpr int kException = 4;
constexpr int kBadArgument = 5;
constexpr int kNotListed = 6;
constexpr int kFirstSendFailed = 7;

// The longest name Spout can carry: a renamed sender `<name>_<n>` (up to 11
// more bytes) gets `<name>_<n>_Count_Semaphore` (16 more) built in 256 bytes
// with sprintf_s, which aborts the process on overflow: 255 - 27 = 228. Must
// match `sp_gpu::SPOUT_NAME_MAX_LEN`.
constexpr std::size_t kMaxNameLen = 228;

struct Sender {
    spoutDX dx;
    // The name asked for. Spout may register another one (see send).
    char name[256] = {};
    // The first SendTexture registered `name` and Spout lists it.
    bool registered = false;
    // Once refused, the code every later send returns (kOk = not refused).
    int refused = kOk;
};

// Refuse `sender` for good with `code`: release its shared texture and any
// registration its first send made. spoutDX::ReleaseSender releases a
// completed registration; a half-made one (the name listed and its info map
// made by this object, then a later step failed) is this object's own too, so
// it is released here. A name listed with no info map is dropped by the next
// CleanSenders of any Spout program.
int Refuse(Sender* sender, int code) {
    // The name spoutDX tried: ours, or `<ours>_<n>` after a rename.
    const std::string attempted = sender->dx.GetName();
    sender->dx.ReleaseSender();
    if (!attempted.empty() && sender->dx.sendernames.FindSender(attempted.c_str())) {
        sender->dx.sendernames.ReleaseSenderName(attempted.c_str());
    }
    sender->refused = code;
    return code;
}

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
        strnlen(name, kMaxNameLen + 1) > kMaxNameLen || std::strchr(name, '\\') != nullptr) {
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
// send registers the sender. It is refused for good, its registration
// released at once, when Spout registered another name (a sender took ours
// in between: kRenamed), did not list it (its sender list is full:
// kNotListed), or the first send failed (kFirstSendFailed: a retry would
// meet its own half-made registration and be renamed). A list that cannot be
// read now (its 67 ms lock) is checked again at the next send.
int spout_sender_send(void* handle, ID3D11Texture2D* texture) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || texture == nullptr) {
        return kBadArgument;
    }
    if (sender->refused != kOk) {
        return sender->refused;
    }
    try {
        if (!sender->dx.SendTexture(texture)) {
            return sender->registered ? kFailed : Refuse(sender, kFirstSendFailed);
        }
        if (!sender->registered) {
            if (std::strcmp(sender->dx.GetName(), sender->name) != 0) {
                return Refuse(sender, kRenamed);
            }
            std::set<std::string> listed;
            if (!sender->dx.sendernames.GetSenderNames(&listed)) {
                return kOk;
            }
            if (listed.count(sender->name) == 0) {
                return Refuse(sender, kNotListed);
            }
            sender->registered = true;
        }
        return kOk;
    } catch (...) {
        return kException;
    }
}

// The size Spout's sender shares (0 x 0 before the first send and once
// refused). Two plain reads: nothing here can throw.
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
