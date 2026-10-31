//! Owned bindings to the official libdave 1.2.1 C API.
//!
//! `DaveLibrary::load` shares one libdave per process.
//! Handles stay on their creating thread. Pass complete encoded frames before
//! RTP packetization and reassemble incoming frames before decryption.
//! Failures carry no detail so native MLS strings never reach callers.

use crate::capture::Codec;
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

type Raw = *mut c_void;
type Result<T> = std::result::Result<T, ()>;
type FailureCallback = unsafe extern "C" fn(*const c_char, *const c_char, Raw);
type LogCallback = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char);
type BufferFn = unsafe extern "C" fn(Raw, *mut *mut u8, *mut usize);
type RosterFn = unsafe extern "C" fn(Raw, *mut *mut u64, *mut usize);
type SignatureFn = unsafe extern "C" fn(Raw, u64, *mut *mut u8, *mut usize);

// Signatures match include/dave/dave.h. C enums use c_int, C bool uses bool,
// size_t uses usize, and each opaque handle is passed as a pointer.
macro_rules! api {
    ($($field:ident: $ty:ty = $symbol:literal),* $(,)?) => {
        pub struct DaveLibrary {
            $($field: $ty,)*
            // Function pointers and all owned handles must outlive their library.
            _library: Library,
        }

        impl DaveLibrary {
            /// Load the pinned libdave 1.2.1 shared object.
            /// The identity bridge requires its shipped C++ headers and ABI,
            /// including libstdc++ on Linux. Other versions are unsupported.
            /// Suppresses native process-wide logs, which can contain MLS bytes.
            fn open() -> Result<Arc<Self>> {
                // SAFETY: loading native code requires a trusted library. All
                // symbols below use the official 1.2.1 C ABI and remain loaded.
                unsafe {
                    let library = Library::new(if cfg!(target_os = "windows") { "libdave.dll" } else { "libdave.so" }).map_err(|_| ())?;
                    $(let $field = *library.get::<$ty>(concat!($symbol, "\0").as_bytes())
                        .map_err(|_| ())?;)*
                    let api = Arc::new(Self { $($field,)* _library: library });
                    (api.set_log_sink)(Some(discard_log));
                    Ok(api)
                }
            }
        }
    };
}

api! {
    set_log_sink: unsafe extern "C" fn(Option<LogCallback>) = "daveSetLogSinkCallback",
    max_version: unsafe extern "C" fn() -> u16 = "daveMaxSupportedProtocolVersion",
    free: unsafe extern "C" fn(Raw) = "daveFree",
    session_create: unsafe extern "C" fn(Raw, *const c_char, Option<FailureCallback>, Raw) -> Raw = "daveSessionCreate",
    session_destroy: unsafe extern "C" fn(Raw) = "daveSessionDestroy",
    session_version: unsafe extern "C" fn(Raw) -> u16 = "daveSessionGetProtocolVersion",
    external_sender: unsafe extern "C" fn(Raw, *const u8, usize) = "daveSessionSetExternalSender",
    key_package: BufferFn = "daveSessionGetMarshalledKeyPackage",
    proposals: unsafe extern "C" fn(Raw, *const u8, usize, *const *const c_char, usize, *mut *mut u8, *mut usize) = "daveSessionProcessProposals",
    commit: unsafe extern "C" fn(Raw, *const u8, usize) -> Raw = "daveSessionProcessCommit",
    welcome: unsafe extern "C" fn(Raw, *const u8, usize, *const *const c_char, usize) -> Raw = "daveSessionProcessWelcome",
    ratchet: unsafe extern "C" fn(Raw, *const c_char) -> Raw = "daveSessionGetKeyRatchet",
    ratchet_destroy: unsafe extern "C" fn(Raw) = "daveKeyRatchetDestroy",
    commit_failed: unsafe extern "C" fn(Raw) -> bool = "daveCommitResultIsFailed",
    commit_ignored: unsafe extern "C" fn(Raw) -> bool = "daveCommitResultIsIgnored",
    commit_roster: RosterFn = "daveCommitResultGetRosterMemberIds",
    commit_signature: SignatureFn = "daveCommitResultGetRosterMemberSignature",
    commit_destroy: unsafe extern "C" fn(Raw) = "daveCommitResultDestroy",
    welcome_roster: RosterFn = "daveWelcomeResultGetRosterMemberIds",
    welcome_signature: SignatureFn = "daveWelcomeResultGetRosterMemberSignature",
    welcome_destroy: unsafe extern "C" fn(Raw) = "daveWelcomeResultDestroy",
    encryptor_create: unsafe extern "C" fn() -> Raw = "daveEncryptorCreate",
    encryptor_destroy: unsafe extern "C" fn(Raw) = "daveEncryptorDestroy",
    encryptor_ratchet: unsafe extern "C" fn(Raw, Raw) = "daveEncryptorSetKeyRatchet",
    encryptor_passthrough: unsafe extern "C" fn(Raw, bool) = "daveEncryptorSetPassthroughMode",
    assign_codec: unsafe extern "C" fn(Raw, u32, c_int) = "daveEncryptorAssignSsrcToCodec",
    ciphertext_size: unsafe extern "C" fn(Raw, c_int, usize) -> usize = "daveEncryptorGetMaxCiphertextByteSize",
    encryptor_has_key: unsafe extern "C" fn(Raw) -> bool = "daveEncryptorHasKeyRatchet",
    encrypt: unsafe extern "C" fn(Raw, c_int, u32, *const u8, usize, *mut u8, usize, *mut usize) -> c_int = "daveEncryptorEncrypt",
    decryptor_create: unsafe extern "C" fn() -> Raw = "daveDecryptorCreate",
    decryptor_destroy: unsafe extern "C" fn(Raw) = "daveDecryptorDestroy",
    decryptor_ratchet: unsafe extern "C" fn(Raw, Raw) = "daveDecryptorTransitionToKeyRatchet",
    decryptor_passthrough: unsafe extern "C" fn(Raw, bool) = "daveDecryptorTransitionToPassthroughMode",
    plaintext_size: unsafe extern "C" fn(Raw, c_int, usize) -> usize = "daveDecryptorGetMaxPlaintextByteSize",
    decrypt: unsafe extern "C" fn(Raw, c_int, *const u8, usize, *mut u8, usize, *mut usize) -> c_int = "daveDecryptorDecrypt",
}

// Never forward libdave's verbose MLS/key material to application logs.
unsafe extern "C" fn discard_log(_: c_int, _: *const c_char, _: c_int, _: *const c_char) {}

impl DaveLibrary {
    /// Resolve libdave through the dynamic loader, once per process.
    pub fn load() -> Result<Arc<Self>> {
        static LIBRARY: OnceLock<Result<Arc<DaveLibrary>>> = OnceLock::new();
        LIBRARY.get_or_init(Self::open).clone()
    }

    pub fn max_supported_protocol_version(&self) -> u16 {
        // SAFETY: the function pointer remains valid while self owns the library.
        unsafe { (self.max_version)() }
    }
}

// NonNull deliberately leaves native handles !Send and !Sync. No libdave
// thread-safety guarantee is assumed. All mutation requires &mut self.
struct Owned {
    raw: NonNull<c_void>,
    api: Arc<DaveLibrary>,
    destroy: unsafe extern "C" fn(Raw),
}

impl Owned {
    fn new(raw: Raw, api: &Arc<DaveLibrary>, destroy: unsafe extern "C" fn(Raw)) -> Result<Self> {
        Ok(Self {
            raw: NonNull::new(raw).ok_or(())?,
            api: api.clone(),
            destroy,
        })
    }

    fn ptr(&self) -> Raw {
        self.raw.as_ptr()
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner, using the matching destroy function
        // before its Arc can unload the library.
        unsafe { (self.destroy)(self.ptr()) }
    }
}

// C output arrays are copied and freed even when validation fails.
unsafe fn take_array<T: Copy>(api: &DaveLibrary, data: *mut T, len: usize) -> Result<Vec<T>> {
    let result = if len == 0 {
        Ok(Vec::new())
    } else if data.is_null() || len > (isize::MAX as usize) / std::mem::size_of::<T>() {
        Err(())
    } else {
        // SAFETY: the official API supplies len initialized, aligned elements.
        Ok(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
    };
    // SAFETY: output allocation belongs to libdave, including null/empty buffers.
    unsafe { (api.free)(data.cast()) };
    result
}

/// libdave interprets IDs as decimal uint64 values internally.
fn user_id(value: &str) -> Result<CString> {
    if !value.bytes().all(|b| b.is_ascii_digit()) || value.parse::<u64>().is_err() {
        return Err(());
    }
    CString::new(value).map_err(|_| ())
}

fn recognized_users(users: &[&str]) -> Result<Vec<CString>> {
    users.iter().map(|id| user_id(id)).collect()
}

// Only flags the failure. Native reason strings are never read.
unsafe extern "C" fn mls_failure(_: *const c_char, _: *const c_char, context: Raw) {
    // SAFETY: Session keeps this boxed flag alive through native destroy.
    unsafe { &*context.cast::<AtomicBool>() }.store(true, Ordering::Relaxed);
}

/// Roster changes returned by MLS, including each member's signature key.
/// An empty signature in a commit delta denotes a removed member.
pub struct RosterMember {
    pub user_id: u64,
    pub signature: Vec<u8>,
}

unsafe extern "C" {
    fn suncordDaveIdentityCreate() -> Raw;
    fn suncordDaveIdentityDestroy(identity: Raw);
    fn suncordDaveSessionInit(
        session: Raw,
        identity: Raw,
        version: u16,
        group: u64,
        user: *const c_char,
    ) -> bool;
}

struct SharedIdentity {
    raw: NonNull<c_void>,
    user: CString,
    // The key's C++ destructor belongs to this library.
    api: Arc<DaveLibrary>,
}

impl Drop for SharedIdentity {
    fn drop(&mut self) {
        unsafe { suncordDaveIdentityDestroy(self.raw.as_ptr()) };
    }
}

/// One ephemeral signing identity for all related voice and Go Live sessions.
/// Create once per call and clone for each connection. Nothing is persisted.
#[derive(Clone)]
pub struct Identity(Rc<SharedIdentity>);

impl Identity {
    pub fn new(user: &str) -> std::result::Result<Self, String> {
        let api = DaveLibrary::load().map_err(|_| "DAVE library unavailable")?;
        let user = user_id(user).map_err(|_| "DAVE user ID must be a decimal uint64")?;
        let raw = NonNull::new(unsafe { suncordDaveIdentityCreate() })
            .ok_or("DAVE identity allocation failed")?;
        Ok(Self(Rc::new(SharedIdentity { raw, user, api })))
    }
}

pub struct Session {
    // Field order keeps the callback context alive during session destruction.
    handle: Owned,
    failure: Box<AtomicBool>,
    identity: Identity,
}

impl Session {
    /// A session for the identity's user. Version 0 (passthrough) is rejected.
    pub fn new(identity: &Identity, group_id: u64, version: u16) -> Result<Self> {
        if version == 0 {
            return Err(());
        }
        let api = &identity.0.api;
        let failure = Box::new(AtomicBool::new(false));
        // SAFETY: callback context has a stable address until after native destroy.
        let raw = unsafe {
            (api.session_create)(
                ptr::null_mut(),
                ptr::null(),
                Some(mls_failure),
                ptr::from_ref(&*failure).cast_mut().cast(),
            )
        };
        let session = Self {
            handle: Owned::new(raw, api, api.session_destroy)?,
            failure,
            identity: identity.clone(),
        };
        // SAFETY: owned session, identity and C string are valid throughout the call.
        let initialized = unsafe {
            suncordDaveSessionInit(
                session.handle.ptr(),
                identity.0.raw.as_ptr(),
                version,
                group_id,
                identity.0.user.as_ptr(),
            )
        };
        session.check()?;
        initialized.then_some(session).ok_or(())
    }

    /// Fails if libdave reported an MLS failure since the last check.
    fn check(&self) -> Result<()> {
        if self.failure.swap(false, Ordering::Relaxed) {
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn protocol_version(&self) -> u16 {
        // SAFETY: valid live session handle.
        unsafe { (self.handle.api.session_version)(self.handle.ptr()) }
    }

    pub fn set_external_sender(&mut self, credentials: &[u8]) -> Result<()> {
        // SAFETY: input slice remains alive and is copied by libdave.
        unsafe {
            (self.handle.api.external_sender)(
                self.handle.ptr(),
                credentials.as_ptr(),
                credentials.len(),
            )
        };
        self.check()
    }

    pub fn key_package(&mut self) -> Result<Vec<u8>> {
        let (mut data, mut len) = (ptr::null_mut(), 0);
        // SAFETY: live handle and valid out pointers. Copy/free before reporting failure.
        let result = unsafe {
            (self.handle.api.key_package)(self.handle.ptr(), &mut data, &mut len);
            take_array(&self.handle.api, data, len)
        };
        self.check()?;
        result.and_then(|bytes| (!bytes.is_empty()).then_some(bytes).ok_or(()))
    }

    /// Returns serialized commit/welcome bytes, or None when no commit is needed.
    pub fn process_proposals(
        &mut self,
        proposals: &[u8],
        recognized: &[&str],
    ) -> Result<Option<Vec<u8>>> {
        let users = recognized_users(recognized)?;
        let pointers: Vec<_> = users.iter().map(|id| id.as_ptr()).collect();
        let (mut data, mut len) = (ptr::null_mut(), 0);
        // SAFETY: all input slices/strings and output pointers remain valid for the call.
        let result = unsafe {
            (self.handle.api.proposals)(
                self.handle.ptr(),
                proposals.as_ptr(),
                proposals.len(),
                pointers.as_ptr(),
                pointers.len(),
                &mut data,
                &mut len,
            );
            take_array(&self.handle.api, data, len)
        };
        self.check()?;
        Ok(Some(result?).filter(|bytes| !bytes.is_empty()))
    }

    /// Returns the roster delta, or None for a soft-ignored commit during reset.
    pub fn process_commit(&mut self, commit: &[u8]) -> Result<Option<Vec<RosterMember>>> {
        let api = &self.handle.api;
        // SAFETY: live session, valid input. The returned handle is owned even on failure.
        let raw = unsafe { (api.commit)(self.handle.ptr(), commit.as_ptr(), commit.len()) };
        let result = Owned::new(raw, api, api.commit_destroy);
        let checked = self.check();
        let result = result?;
        // SAFETY: result is a valid commit result, queried before destruction.
        unsafe {
            if (api.commit_ignored)(result.ptr()) {
                return Ok(None);
            }
            checked?;
            if (api.commit_failed)(result.ptr()) {
                return Err(());
            }
            roster(&result, api.commit_roster, api.commit_signature).map(Some)
        }
    }

    /// Success contains the full initial roster.
    pub fn process_welcome(
        &mut self,
        welcome: &[u8],
        recognized: &[&str],
    ) -> Result<Vec<RosterMember>> {
        let users = recognized_users(recognized)?;
        let pointers: Vec<_> = users.iter().map(|id| id.as_ptr()).collect();
        let api = &self.handle.api;
        // SAFETY: stable input arrays. Nullable result is checked and owned immediately.
        let raw = unsafe {
            (api.welcome)(
                self.handle.ptr(),
                welcome.as_ptr(),
                welcome.len(),
                pointers.as_ptr(),
                pointers.len(),
            )
        };
        let result = Owned::new(raw, api, api.welcome_destroy);
        self.check()?;
        // SAFETY: only known roster member IDs are queried.
        unsafe { roster(&result?, api.welcome_roster, api.welcome_signature) }
    }

    fn key_ratchet(&mut self, id: &CString) -> Result<Owned> {
        let api = &self.handle.api;
        // SAFETY: live handle, valid decimal C string, returned handle has unique ownership.
        let raw = unsafe { (api.ratchet)(self.handle.ptr(), id.as_ptr()) };
        let result = Owned::new(raw, api, api.ratchet_destroy);
        self.check()?;
        result
    }

    /// Created without keys. Transition with `transition_sender` before use.
    pub fn encryptor(&self) -> Result<Encryptor> {
        let api = &self.handle.api;
        // SAFETY: matching create/destroy functions from the retained library.
        let handle = Owned::new(
            unsafe { (api.encryptor_create)() },
            api,
            api.encryptor_destroy,
        )?;
        // SAFETY: live handle. Passthrough is never enabled.
        unsafe { (api.encryptor_passthrough)(handle.ptr(), false) };
        Ok(Encryptor { handle })
    }

    /// Bound to one sender. Keep one per remote user across epochs and
    /// transition it with `transition_receiver` before use.
    pub fn decryptor(&self, id: &str) -> Result<Decryptor> {
        let user_id = user_id(id)?;
        let api = &self.handle.api;
        // SAFETY: matching create/destroy functions from the retained library.
        let handle = Owned::new(
            unsafe { (api.decryptor_create)() },
            api,
            api.decryptor_destroy,
        )?;
        // SAFETY: live handle. Plaintext passthrough is never enabled.
        unsafe { (api.decryptor_passthrough)(handle.ptr(), false) };
        Ok(Decryptor {
            handle,
            user_id,
            has_key: false,
        })
    }

    pub fn transition_sender(&mut self, encryptor: &mut Encryptor) -> Result<()> {
        let id = self.identity.0.user.clone();
        let ratchet = self.key_ratchet(&id)?;
        // SAFETY: 1.2.1 bindings_capi.cpp copies the ratchet synchronously, so
        // the original can be destroyed after this call. Media owns its copy.
        unsafe { (self.handle.api.encryptor_ratchet)(encryptor.handle.ptr(), ratchet.ptr()) };
        Ok(())
    }

    pub fn transition_receiver(&mut self, decryptor: &mut Decryptor) -> Result<()> {
        let ratchet = self.key_ratchet(&decryptor.user_id)?;
        // SAFETY: native decryptor copies the ratchet, preserving prior epochs itself.
        unsafe { (self.handle.api.decryptor_ratchet)(decryptor.handle.ptr(), ratchet.ptr()) };
        decryptor.has_key = true;
        Ok(())
    }
}

unsafe fn roster(
    result: &Owned,
    get_ids: RosterFn,
    get_signature: SignatureFn,
) -> Result<Vec<RosterMember>> {
    let (mut ids, mut len) = (ptr::null_mut(), 0);
    // SAFETY: live result of the matching type, valid output pointers.
    unsafe { get_ids(result.ptr(), &mut ids, &mut len) };
    let ids = unsafe { take_array(&result.api, ids, len) }?;
    ids.into_iter()
        .map(|user_id| {
            let (mut data, mut len) = (ptr::null_mut(), 0);
            // SAFETY: user_id came from this result's roster. Arrays are copied/freed.
            let signature = unsafe {
                get_signature(result.ptr(), user_id, &mut data, &mut len);
                take_array(&result.api, data, len)
            }?;
            Ok(RosterMember { user_id, signature })
        })
        .collect()
}

/// libdave codec and media type enum values.
fn native(codec: Codec) -> (c_int, c_int) {
    match codec {
        Codec::Opus => (1, 0),
        Codec::H264 => (4, 1),
        Codec::H265 => (5, 1),
        Codec::Av1 => (6, 1),
    }
}

pub struct Encryptor {
    handle: Owned,
}

impl Encryptor {
    /// Encrypt one complete Opus or encoded video frame. Errors never return plaintext.
    pub fn encrypt(&mut self, ssrc: u32, codec: Codec, frame: &[u8]) -> Result<Vec<u8>> {
        if frame.is_empty() {
            return Err(());
        }
        let api = &self.handle.api;
        let (codec, media) = native(codec);
        // SAFETY: live exclusively borrowed handle, valid frame/output slices.
        unsafe {
            if !(api.encryptor_has_key)(self.handle.ptr()) {
                return Err(());
            }
            (api.assign_codec)(self.handle.ptr(), ssrc, codec);
            let capacity = (api.ciphertext_size)(self.handle.ptr(), media, frame.len());
            let mut output = output_buffer(capacity)?;
            let mut written = 0;
            let code = (api.encrypt)(
                self.handle.ptr(),
                media,
                ssrc,
                frame.as_ptr(),
                frame.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut written,
            );
            finish_frame(code, output, written)
        }
    }
}

pub struct Decryptor {
    handle: Owned,
    user_id: CString,
    has_key: bool,
}

impl Decryptor {
    pub fn decrypt(&mut self, codec: Codec, frame: &[u8]) -> Result<Vec<u8>> {
        if !self.has_key || frame.is_empty() {
            return Err(());
        }
        let api = &self.handle.api;
        let media = native(codec).1;
        // SAFETY: live exclusively borrowed handle, valid frame/output slices.
        unsafe {
            let capacity = (api.plaintext_size)(self.handle.ptr(), media, frame.len());
            let mut output = output_buffer(capacity)?;
            let mut written = 0;
            let code = (api.decrypt)(
                self.handle.ptr(),
                media,
                frame.as_ptr(),
                frame.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut written,
            );
            finish_frame(code, output, written)
        }
    }
}

fn output_buffer(capacity: usize) -> Result<Vec<u8>> {
    if capacity == 0 {
        return Err(());
    }
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|_| ())?;
    output.resize(capacity, 0);
    Ok(output)
}

fn finish_frame(code: c_int, mut output: Vec<u8>, written: usize) -> Result<Vec<u8>> {
    if code != 0 || written == 0 || written > output.len() {
        return Err(());
    }
    output.truncate(written);
    Ok(output)
}
