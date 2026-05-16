//! DRM authentication and security model

use alloc::vec::Vec;
use spin::Mutex;

/// DRM authentication token
#[derive(Debug, Clone, PartialEq)]
pub struct DrmAuthToken {
    pub session_id: u32,
    pub magic: u32,
    pub authenticated: bool,
    pub capabilities: DrmCapabilities,
}

impl DrmAuthToken {
    /// Create a new unauthenticated token
    pub fn new(session_id: u32) -> Self {
        static NEXT_MAGIC: Mutex<u32> = Mutex::new(1);
        let magic = {
            let mut next = NEXT_MAGIC.lock();
            let current = *next;
            *next += 1;
            current
        };

        Self {
            session_id,
            magic,
            authenticated: false,
            capabilities: DrmCapabilities::default(),
        }
    }

    /// Create an authenticated token
    pub fn new_authenticated(magic: u32) -> Self {
        Self {
            session_id: 0, // Will be set by session manager
            magic,
            authenticated: true,
            capabilities: DrmCapabilities::all(),
        }
    }

    /// Authenticate the token
    pub fn authenticate(&mut self, provided_magic: u32) -> Result<(), AuthError> {
        if self.magic == provided_magic {
            self.authenticated = true;
            self.capabilities = DrmCapabilities::all();
            Ok(())
        } else {
            Err(AuthError::InvalidMagic)
        }
    }

    /// Check if token has specific capability
    pub fn has_capability(&self, cap: DrmCapability) -> bool {
        self.authenticated && self.capabilities.has(cap)
    }
}

/// DRM capabilities that can be granted to clients
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrmCapabilities {
    pub bits: u32,
}

impl DrmCapabilities {
    pub fn new(bits: u32) -> Self {
        Self { bits }
    }

    pub fn default() -> Self {
        Self { bits: 0 }
    }

    pub fn all() -> Self {
        Self { bits: u32::MAX }
    }

    pub fn has(&self, cap: DrmCapability) -> bool {
        self.bits & (cap as u32) != 0
    }

    pub fn add(&mut self, cap: DrmCapability) {
        self.bits |= cap as u32;
    }

    pub fn remove(&mut self, cap: DrmCapability) {
        self.bits &= !(cap as u32);
    }
}

/// Individual DRM capabilities
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmCapability {
    /// Can read device information
    Read = 1 << 0,
    /// Can perform mode setting
    ModeSet = 1 << 1,
    /// Can create and manage framebuffers
    Framebuffer = 1 << 2,
    /// Can use atomic operations
    Atomic = 1 << 3,
    /// Can become DRM master
    Master = 1 << 4,
    /// Can authenticate other clients
    Auth = 1 << 5,
    /// Can access universal planes
    UniversalPlanes = 1 << 6,
    /// Can access advanced features
    Advanced = 1 << 7,
}

/// Authentication errors
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuthError {
    InvalidMagic,
    AlreadyAuthenticated,
    NotAuthenticated,
    InsufficientPermissions,
    SessionExpired,
}

/// DRM Master privilege holder
#[derive(Debug, Clone)]
pub struct DrmMaster {
    pub token: DrmAuthToken,
    pub acquired_at: u64, // Timestamp
}

impl DrmMaster {
    pub fn new(token: DrmAuthToken) -> Self {
        Self {
            token,
            acquired_at: 0, // Would be set to current timestamp
        }
    }

    /// Check if this master can authorize operations
    pub fn can_authorize(&self, operation: DrmOperation) -> bool {
        match operation {
            DrmOperation::ModeSet => self.token.has_capability(DrmCapability::ModeSet),
            DrmOperation::CreateFramebuffer => self.token.has_capability(DrmCapability::Framebuffer),
            DrmOperation::AtomicCommit => self.token.has_capability(DrmCapability::Atomic),
            DrmOperation::Authenticate => self.token.has_capability(DrmCapability::Auth),
        }
    }
}

/// Operations that require authorization
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmOperation {
    ModeSet,
    CreateFramebuffer,
    AtomicCommit,
    Authenticate,
}

/// Authentication manager
pub struct AuthManager {
    active_sessions: Vec<DrmAuthToken>,
    master_session: Option<u32>,
    next_session_id: u32,
}

impl AuthManager {
    pub fn new() -> Self {
        Self {
            active_sessions: Vec::new(),
            master_session: None,
            next_session_id: 1,
        }
    }

    /// Create a new session
    pub fn create_session(&mut self) -> DrmAuthToken {
        let session_id = self.next_session_id;
        self.next_session_id += 1;

        let token = DrmAuthToken::new(session_id);
        self.active_sessions.push(token.clone());
        token
    }

    /// Authenticate a session with magic number
    pub fn authenticate_session(&mut self, session_id: u32, magic: u32) -> Result<(), AuthError> {
        for session in &mut self.active_sessions {
            if session.session_id == session_id {
                return session.authenticate(magic);
            }
        }
        Err(AuthError::InvalidMagic)
    }

    /// Set a session as master
    pub fn set_master(&mut self, session_id: u32) -> Result<(), AuthError> {
        // Check if session exists and is authenticated
        let session_exists = self.active_sessions.iter()
            .any(|s| s.session_id == session_id && s.authenticated);

        if !session_exists {
            return Err(AuthError::NotAuthenticated);
        }

        // Only one master at a time
        if self.master_session.is_some() {
            return Err(AuthError::InsufficientPermissions);
        }

        self.master_session = Some(session_id);
        Ok(())
    }

    /// Drop master privilege
    pub fn drop_master(&mut self, session_id: u32) -> Result<(), AuthError> {
        if self.master_session == Some(session_id) {
            self.master_session = None;
            Ok(())
        } else {
            Err(AuthError::InsufficientPermissions)
        }
    }

    /// Check if session is master
    pub fn is_master(&self, session_id: u32) -> bool {
        self.master_session == Some(session_id)
    }

    /// Check if session can perform operation
    pub fn can_perform(&self, session_id: u32, operation: DrmOperation) -> bool {
        // Find the session
        if let Some(session) = self.active_sessions.iter().find(|s| s.session_id == session_id) {
            if !session.authenticated {
                return false;
            }

            // Check if operation requires master
            let requires_master = matches!(operation,
                DrmOperation::ModeSet | DrmOperation::AtomicCommit);

            if requires_master && !self.is_master(session_id) {
                return false;
            }

            // Check capability
            let required_cap = match operation {
                DrmOperation::ModeSet => DrmCapability::ModeSet,
                DrmOperation::CreateFramebuffer => DrmCapability::Framebuffer,
                DrmOperation::AtomicCommit => DrmCapability::Atomic,
                DrmOperation::Authenticate => DrmCapability::Auth,
            };

            session.has_capability(required_cap)
        } else {
            false
        }
    }

    /// Close a session
    pub fn close_session(&mut self, session_id: u32) {
        self.active_sessions.retain(|s| s.session_id != session_id);
        if self.master_session == Some(session_id) {
            self.master_session = None;
        }
    }

    /// Get session by ID
    pub fn get_session(&self, session_id: u32) -> Option<&DrmAuthToken> {
        self.active_sessions.iter().find(|s| s.session_id == session_id)
    }

    /// Get mutable session by ID
    pub fn get_session_mut(&mut self, session_id: u32) -> Option<&mut DrmAuthToken> {
        self.active_sessions.iter_mut().find(|s| s.session_id == session_id)
    }
}

/// Global authentication manager
static AUTH_MANAGER: Mutex<AuthManager> = Mutex::new(AuthManager {
    active_sessions: Vec::new(),
    master_session: None,
    next_session_id: 1,
});

/// Global authentication interface
pub fn create_session() -> DrmAuthToken {
    AUTH_MANAGER.lock().create_session()
}

pub fn authenticate_session(session_id: u32, magic: u32) -> Result<(), AuthError> {
    AUTH_MANAGER.lock().authenticate_session(session_id, magic)
}

pub fn set_master(session_id: u32) -> Result<(), AuthError> {
    AUTH_MANAGER.lock().set_master(session_id)
}

pub fn drop_master(session_id: u32) -> Result<(), AuthError> {
    AUTH_MANAGER.lock().drop_master(session_id)
}

pub fn is_master(session_id: u32) -> bool {
    AUTH_MANAGER.lock().is_master(session_id)
}

pub fn can_perform(session_id: u32, operation: DrmOperation) -> bool {
    AUTH_MANAGER.lock().can_perform(session_id, operation)
}

pub fn close_session(session_id: u32) {
    AUTH_MANAGER.lock().close_session(session_id)
}