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
// their warnings are silenced, so build.rs's /W4 /WX judges only the shim.
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
// #251: the GPU did not finish the copy within the bound (or its query
// failed): the frame was NOT announced (one lost frame).
constexpr int kCopyNotDone = 6;
// #251 (19.10.2026): SyncedDX::SendSynced's hand-off modes (sp-gpu
// `spout.rs`'s `shim_send_mode`, which must match).
constexpr int kSendSynced = 0;
constexpr int kSendPlain = 1;
constexpr int kSendReleaseThenWait = 2;
static_assert(kSendSynced != kSendPlain && kSendPlain != kSendReleaseThenWait &&
                  kSendSynced != kSendReleaseThenWait,
              "the hand-off modes are distinct");

// The longest name Spout can carry: a renamed sender `<name>_<n>` (up to 11
// more bytes) gets `<name>_<n>_Count_Semaphore` (16 more) built in 256 bytes
// with sprintf_s, which aborts the process on overflow: 255 - 27 = 228. Must
// match `sp_gpu::SPOUT_NAME_MAX_LEN`.
constexpr std::size_t kMaxNameLen = 228;

// #251: spoutDX's own SendTexture copies into Spout's shared texture,
// flushes, announces the frame (SetNewFrame) and lets receivers in
// (AllowTextureAccess) at once. Flush only submits the copy, so a receiver
// could take the frame while the GPU still wrote it: the wall showed the new
// picture for one refresh, then the old one again. SendSynced does the same
// steps on the same members, but waits until the GPU has done the copy
// (`done`, an event query on this device) before the announce, still under
// the sender's access mutex. The vendored SDK stays unmodified.
class SyncedDX : public spoutDX {
   public:
    // 0 sent (or Spout skipped the copy: a receiver held the mutex over
    // 67 ms, as SendTexture), 3 no sender, 6 the copy not done within
    // `bound_ms` (not announced; the mutex is let go all the same). Throws
    // what the SDK throws. #251 (19.10.2026) `mode` (sp-gpu `spout.rs`'s
    // `shim_send_mode`): kSendSynced waits for the copy holding the mutex,
    // kSendPlain is SendTexture, kSendReleaseThenWait lets receivers in
    // before the wait and announces only a done copy.
    int SendSynced(ID3D11Texture2D* texture, ID3D11Query* done, unsigned long long bound_ms,
                   int mode) {
        D3D11_TEXTURE2D_DESC desc;
        ZeroMemory(&desc, sizeof(desc));
        texture->GetDesc(&desc);
        if (desc.Width == 0 || desc.Height == 0 || !OpenDirectX11() ||
            !CheckSender(desc.Width, desc.Height, static_cast<DWORD>(desc.Format))) {
            return kFailed;
        }
        if (!frame.CheckTextureAccess(m_pSharedTexture)) {
            return kOk;
        }
        m_pImmediateContext->CopyResource(m_pSharedTexture, texture);
        if (mode == kSendPlain) {
            // spoutDX's own SendTexture: announce now, never wait.
            m_pImmediateContext->Flush();
            frame.SetNewFrame();
            frame.AllowTextureAccess(m_pSharedTexture);
            return kOk;
        }
        m_pImmediateContext->End(done);
        m_pImmediateContext->Flush();
        if (mode == kSendReleaseThenWait) {
            // #251 (19.10.2026): let receivers in before the wait, so none
            // waits on this device's GPU queue; announce only a done copy.
            frame.AllowTextureAccess(m_pSharedTexture);
        }
        int result = WaitDone(done, bound_ms);
        if (result == kOk) {
            frame.SetNewFrame();
        }
        if (mode != kSendReleaseThenWait) {
            frame.AllowTextureAccess(m_pSharedTexture);
        }
        return result;
    }

   private:
    // Spins until the GPU has done the copy `done` follows: kOk, or
    // kCopyNotDone past `bound_ms` (or a failed query).
    int WaitDone(ID3D11Query* done, unsigned long long bound_ms) {
        const unsigned long long start = GetTickCount64();
        for (;;) {
            BOOL finished = FALSE;
            const UINT size = static_cast<UINT>(sizeof(finished));
            const HRESULT hr = m_pImmediateContext->GetData(done, &finished, size, 0);
            if (hr == S_OK && finished) {
                return kOk;
            }
            if (FAILED(hr) || GetTickCount64() - start > bound_ms) {
                return kCopyNotDone;
            }
            SwitchToThread();
        }
    }
};

struct Sender {
    SyncedDX dx;
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

// #243: put the sender's own name back in Spout's names list
// (RegisterSenderName without a new name: no `_1`, the same info map and
// shared texture), after another program's clean-up dropped it. 0 listed,
// 3 not (the list's lock or the list full), 4 exception.
int spout_sender_relist(void* handle) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr) {
        return kBadArgument;
    }
    try {
        return sender->dx.sendernames.RegisterSenderName(sender->name, false) ? kOk : kFailed;
    } catch (...) {
        return kException;
    }
}

// SyncedDX::SendSynced: copy `texture` (on the sender's device) into Spout's
// own shared texture under the sender's named mutex, wait until the GPU has
// done it (`done`, an event query of that device, at most `bound_ms`), then
// announce the frame; the first call registers the sender. 0 sent, 3 failed,
// 4 exception, 6 the copy not done within the bound (not announced).
int spout_sender_send_synced(void* handle, ID3D11Texture2D* texture, ID3D11Query* done,
                             unsigned long long bound_ms, int mode) {
    Sender* sender = static_cast<Sender*>(handle);
    if (sender == nullptr || texture == nullptr || done == nullptr) {
        return kBadArgument;
    }
    try {
        return sender->dx.SendSynced(texture, done, bound_ms, mode);
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
