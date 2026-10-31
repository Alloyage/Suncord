//! MLS discovery framing and per-connection DAVE state.
use super::USERS;
use crate::dave;
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::Instant;

// Only a framing reader, never MLS authentication. Candidates originate from
// this stream's authenticated WSS. Libdave validates subsequent group data.
// RFC 9420 variable vectors, PublicMessage, Add/Remove, and referenced commits.
struct MlsReader<'a>(&'a [u8]);
impl<'a> MlsReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.0.len() {
            return Err("Malformed MLS discovery frame".into());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn short(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn vector(&mut self) -> Result<&'a [u8], String> {
        let first = self.byte()?;
        let width = match first >> 6 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return Err("Invalid MLS vector length".into()),
        };
        let mut len = usize::from(first & 63);
        for byte in self.take(width - 1)? {
            len = (len << 8) | usize::from(*byte);
        }
        if (width == 2 && len < 64) || (width == 4 && len < 16384) {
            return Err("Noncanonical MLS vector length".into());
        }
        self.take(len)
    }
    fn leaf(&mut self) -> Result<(), String> {
        self.vector()?; // encryption key
        self.vector()?; // signing key
        if self.short()? != 1 {
            return Err("Unsupported MLS credential".into());
        }
        self.vector()?; // basic identity
        for _ in 0..5 {
            self.vector()?;
        } // capabilities
        match self.byte()? {
            1 => {
                self.take(16)?;
            } // lifetime
            2 => {}
            3 => {
                self.vector()?;
            } // parent hash
            _ => return Err("Invalid MLS leaf source".into()),
        }
        self.vector()?; // extensions
        self.vector()?; // signature
        Ok(())
    }
    fn public(&mut self, content: u8) -> Result<u64, String> {
        if self.short()? != 1 || self.short()? != 1 {
            return Err("Expected MLS 1.0 PublicMessage".into());
        }
        let group = self.vector()?;
        let group = u64::from_be_bytes(
            group
                .try_into()
                .map_err(|_| "MLS group ID must contain eight bytes")?,
        );
        self.take(8)?; // epoch
        let sender = self.byte()?;
        match sender {
            1 | 2 => {
                self.take(4)?;
            }
            3 | 4 => {}
            _ => return Err("Invalid MLS sender type".into()),
        }
        self.vector()?; // authenticated data
        if self.byte()? != content {
            return Err("Unexpected MLS content type".into());
        }
        if content == 2 {
            if sender != 2 {
                return Err("Expected external MLS proposal".into());
            }
            match self.short()? {
                1 => {
                    if self.short()? != 1 {
                        return Err("Invalid MLS key package version".into());
                    }
                    self.short()?; // cipher suite, validated by libdave later
                    self.vector()?; // init key
                    self.leaf()?;
                    self.vector()?; // extensions
                    self.vector()?; // signature
                }
                3 => {
                    self.take(4)?;
                }
                _ => return Err("Unsupported MLS discovery proposal".into()),
            }
        } else {
            let mut refs = MlsReader(self.vector()?);
            while !refs.0.is_empty() {
                if refs.byte()? != 2 {
                    return Err("Expected referenced MLS proposal".into());
                }
                refs.vector()?;
            }
            match self.byte()? {
                0 => {}
                1 => {
                    self.leaf()?;
                    let mut nodes = MlsReader(self.vector()?);
                    while !nodes.0.is_empty() {
                        nodes.vector()?;
                        let mut secrets = MlsReader(nodes.vector()?);
                        while !secrets.0.is_empty() {
                            secrets.vector()?;
                            secrets.vector()?;
                        }
                    }
                }
                _ => return Err("Invalid MLS update path".into()),
            }
        }
        if self.vector()?.is_empty() {
            return Err("Missing MLS signature".into());
        }
        if content == 3 {
            self.vector()?;
        } // confirmation tag
        if sender == 1 {
            self.vector()?;
        } // membership tag
        Ok(group)
    }
}

pub(super) fn discover_group(op: u8, data: &[u8]) -> Result<Option<u64>, String> {
    let mut reader = MlsReader(data);
    if op == 29 {
        reader.take(2)?; // actual transition ID remains available to recovery
        let group = reader.public(3)?;
        if !reader.0.is_empty() {
            return Err("Trailing MLS commit data".into());
        }
        return Ok(Some(group));
    }
    match reader.byte()? {
        1 => {
            reader.vector()?;
            return if reader.0.is_empty() {
                Ok(None)
            } else {
                Err("Trailing MLS revocation data".into())
            };
        }
        0 => {}
        _ => return Err("Invalid MLS proposal operation".into()),
    }
    let mut messages = MlsReader(reader.vector()?);
    if !reader.0.is_empty() {
        return Err("Trailing MLS proposals data".into());
    }
    let mut candidate = None;
    let mut count = 0;
    while !messages.0.is_empty() {
        count += 1;
        if count > USERS {
            return Err("MLS discovery proposal limit exceeded".into());
        }
        let group = messages.public(2)?;
        if candidate.is_some_and(|previous| previous != group) {
            return Err("Inconsistent MLS group candidates".into());
        }
        candidate = Some(group);
    }
    Ok(candidate)
}

pub(super) struct Mls {
    pub(super) session: dave::Session,
    pub(super) encryptor: dave::Encryptor,
    pub(super) decryptors: BTreeMap<String, dave::Decryptor>,
    pub(super) roster: BTreeSet<String>,
    pub(super) pending: Option<(u16, Instant)>,
    pub(super) external: bool,
    pub(super) ready: bool,
    pub(super) generation: u64,
}
impl Mls {
    pub(super) fn new(
        identity: &dave::Identity,
        group: u64,
        version: u16,
        external: Option<&[u8]>,
    ) -> Result<Self, String> {
        let session = dave::Session::new(identity, group, version)
            .map_err(|_| "DAVE initialization failed")?;
        let encryptor = session
            .encryptor()
            .map_err(|_| "DAVE encryptor initialization failed")?;
        let mut mls = Self {
            session,
            encryptor,
            decryptors: BTreeMap::new(),
            roster: BTreeSet::new(),
            pending: None,
            external: false,
            ready: false,
            generation: 0,
        };
        if let Some(sender) = external {
            mls.set_external_sender(sender)?;
        }
        Ok(mls)
    }
    pub(super) fn set_external_sender(&mut self, sender: &[u8]) -> Result<(), String> {
        self.session
            .set_external_sender(sender)
            .map_err(|_| "Invalid MLS external sender")?;
        self.external = true;
        Ok(())
    }
    pub(super) fn roster(
        &mut self,
        members: Vec<dave::RosterMember>,
        full: bool,
        recognized: &BTreeSet<String>,
        own: &str,
    ) -> Result<(), String> {
        if full {
            self.roster.clear();
        }
        if members.len() > USERS {
            return Err("DAVE roster exceeded limit".into());
        }
        for member in members {
            let user = member.user_id.to_string();
            if !full && member.signature.is_empty() {
                self.roster.remove(&user);
            } else {
                if !recognized.contains(&user) {
                    return Err("Unrecognized DAVE member".into());
                }
                self.roster.insert(user);
            }
        }
        if self.roster.len() > USERS {
            return Err("DAVE roster exceeded limit".into());
        }
        self.decryptors.retain(|user, _| self.roster.contains(user));
        for user in &self.roster {
            if user == own {
                continue;
            }
            if !self.decryptors.contains_key(user) {
                self.decryptors.insert(
                    user.clone(),
                    self.session
                        .decryptor(user)
                        .map_err(|_| "DAVE receiver creation failed")?,
                );
            }
            self.session
                .transition_receiver(self.decryptors.get_mut(user).unwrap())
                .map_err(|_| "DAVE receiver transition failed")?;
        }
        Ok(())
    }
    pub(super) fn execute(&mut self, transition: u16) -> Result<(), String> {
        if !self.external || self.pending.map(|p| p.0) != Some(transition) {
            return Err("Invalid DAVE execute transition".into());
        }
        // An epoch reset can execute before a new group has any authenticated
        // members. There is no sender ratchet until a commit or welcome arrives.
        if self.roster.is_empty() {
            self.pending = None;
            self.ready = false;
            return Ok(());
        }
        self.session
            .transition_sender(&mut self.encryptor)
            .map_err(|_| "DAVE sender transition failed")?;
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
        self.ready = true;
        Ok(())
    }
}
