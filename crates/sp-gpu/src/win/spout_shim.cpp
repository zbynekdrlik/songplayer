// The `extern "C"` shim between sp-gpu's `SpoutSender`
// (src/win/spout_sender.rs) and the vendored Spout2 SDK 2.007.017 sender,
// SpoutDX (vendor/spout2) — #223 S1b.
//
// Primitives only: each entry point does one SpoutDX step and reports what
// happened. Every decision (refuse a taken name, confirm or refuse a first
// send) is Rust's: `src/spout_state.rs`, tested on Linux.
//
// Every entry point that can throw catches every C++ exception (code 4);
// `release` runs ~spoutDX (a throw in a destructor terminates, it never
// unwinds). Nothing unwinds into Rust. The codes must match
// `sp_gpu::spout::status` (src/spout.rs).

// The SDK's headers (and the Windows headers they pull in) are not ours:
// their warnings are silenced, so build.rs's /W4 shows only the shim's own.
#pragma warning(push, 0)
#include "SpoutDX.h"
#pragma warning(pop)

#include <cstddef>
#include <cstring>
#include <set>
#include <string>

namespace {

constexpr int kOk = 0;
constexpr int kRenamed = 1;
constexpr int kFailed = 3;
constexpr int kException = 4;
constexpr int kBadArgument = 5;

// The longest name Spout can carry: a renamed sender `<name>_<n>` (up to 11
// more bytes) gets `<name>_<n>_Count_Semaphore` (16 more) built in 256 bytes
// with sprintf_s, which aborts the process on overflow: 255 - 27 = 228. Must
// match `sp_gpu::SPOUT_NAME_MAX_LEN`.
constexpr std::size_t kMaxNameLen = 228;

struct Sender {
    spoutDX dx;
    // The name asked for. Spout may register another one.
    char name[256] = {};
};

}  // namespace

extern "C" {

// A sender object on `device` (the caller keeps the device alive until
// release: spoutDX does not AddRef it) for `name`, nothing registered yet.
// Spout's own clean-up (CleanSenders) has dropped the listed names whose
// sender is gone. NULL with `*status` set (4 exception, 5 bad argument) on
// failure.
void* spout_sender_open(ID3D11Device* device, const char* name, int* status) {
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
        sender->dx.sendernames.CleanSenders();
        *status = kOk;
        return sender;
    } catch (...) {
        delete sender;
        *status = kException;
        return nullptr;
    }
}

// Whether Spout's names list holds the sender's name: 1 yes, 0 no, -1 the
// list could not be read (its 67 ms lock, or an exception).
int spout_sender_listed(void* handle) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr) {
        return -1;
    }
    try {
        std::set<std::string> listed;
        if (!sender->dx.sendernames.GetSenderNames(&listed)) {
            return -1;
        }
        return listed.count(sender->name) > 0 ? 1 : 0;
    } catch (...) {
        return -1;
    }
}

// Give spoutDX the name (SetSenderName): 0 kept, 1 Spout renamed it (a
// sender listed it), 4 exception.
int spout_sender_claim_name(void* handle) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr) {
        return kBadArgument;
    }
    try {
        sender->dx.SetSenderName(sender->name);
        return std::strcmp(sender->dx.GetName(), sender->name) == 0 ? kOk : kRenamed;
    } catch (...) {
        return kException;
    }
}

// spoutDX::SendTexture: copy `texture` (on the sender's device) into Spout's
// own shared texture under the sender's named mutex; the first call
// registers the sender. 0 sent, 3 SendTexture failed, 4 exception.
int spout_sender_send(void* handle, ID3D11Texture2D* texture) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || texture == nullptr) {
        return kBadArgument;
    }
    try {
        return sender->dx.SendTexture(texture) ? kOk : kFailed;
    } catch (...) {
        return kException;
    }
}

// What a send registered: whether spoutDX holds a registration
// (IsInitialized) and whether it is under the name asked for. Two plain
// reads: nothing here can throw.
void spout_sender_state(void* handle, int* initialized, int* name_matches) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || initialized == nullptr || name_matches == nullptr) {
        return;
    }
    *initialized = sender->dx.IsInitialized() ? 1 : 0;
    *name_matches = std::strcmp(sender->dx.GetName(), sender->name) == 0 ? 1 : 0;
}

// Release whatever this sender registered: spoutDX::ReleaseSender releases a
// completed registration and its shared texture; a half-made one (the name
// listed and its info map made by this object, then a later step failed) is
// this object's own too (FindSender looks only at this object's maps), so it
// is released here. Another sender's listing is never touched. A name listed
// with no info map is dropped by the next CleanSenders of any Spout program.
// 0 done, 4 exception.
int spout_sender_refuse(void* handle) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr) {
        return kBadArgument;
    }
    try {
        // The name spoutDX tried: ours, or `<ours>_<n>` after a rename.
        const std::string attempted = sender->dx.GetName();
        sender->dx.ReleaseSender();
        if (!attempted.empty() && sender->dx.sendernames.FindSender(attempted.c_str())) {
            sender->dx.sendernames.ReleaseSenderName(attempted.c_str());
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

// Release the sender object: ~spoutDX unregisters it (off Spout's list, its
// info map closed) and releases its context reference. NULL is a no-op.
void spout_sender_release(void* handle) {
    delete static_cast<Sender*>(handle);
}

}  // extern "C"
