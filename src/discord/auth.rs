use super::models::UserDto;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Accounts {
    active: Option<String>,
    users: Vec<UserDto>,
}

struct AuthInner {
    token: Mutex<Option<Zeroizing<String>>>,
    directory: PathBuf,
    accounts: Mutex<Accounts>,
}

#[derive(Clone)]
pub struct AuthStore {
    inner: Arc<AuthInner>,
}

impl AuthStore {
    pub fn new(directory: PathBuf) -> Self {
        let mut accounts: Accounts = std::fs::read(directory.join("accounts.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        accounts.users.retain(|user| valid_account(&user.id));
        if !accounts
            .users
            .iter()
            .any(|user| Some(&user.id) == accounts.active.as_ref())
        {
            accounts.active = accounts.users.first().map(|user| user.id.clone());
        }
        Self {
            inner: Arc::new(AuthInner {
                directory,
                token: Mutex::new(None),
                accounts: Mutex::new(accounts),
            }),
        }
    }

    pub fn active_account(&self) -> Option<String> {
        self.inner.accounts.lock().active.clone()
    }

    pub fn accounts(&self) -> Vec<UserDto> {
        self.inner.accounts.lock().users.clone()
    }

    pub fn read_account_token(&self, account: &str) -> Result<Option<Zeroizing<String>>, String> {
        if !valid_account(account) {
            return Err("Invalid account.".into());
        }
        let path = self.inner.directory.join(account);
        match std::fs::read_to_string(path) {
            // An empty file records sign-out if the keychain could not be cleared.
            Ok(raw) => {
                let raw = Zeroizing::new(raw);
                if raw.is_empty() {
                    return Ok(None);
                }
                return if valid_token(&raw) {
                    Ok(Some(raw))
                } else {
                    Err("The saved sign-in is unreadable. Sign in again.".into())
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Could not read the saved sign-in.".into()),
        }
        Ok(read_keyring(&account_key(account))
            .map(Zeroizing::new)
            .filter(|raw| valid_token(raw)))
    }

    pub fn token(&self) -> Option<Zeroizing<String>> {
        self.inner
            .token
            .lock()
            .as_ref()
            .map(|token| Zeroizing::new(token.to_string()))
    }

    pub fn use_token(&self, token: &str) {
        *self.inner.token.lock() = Some(Zeroizing::new(token.to_string()));
    }

    pub fn save_account(&self, user: &UserDto, token: &str) -> Result<(), String> {
        if !valid_account(&user.id) {
            return Err("Invalid account.".into());
        }
        let path = self.inner.directory.join(&user.id);
        if path.exists() || write_keyring(&account_key(&user.id), token).is_err() {
            write_fallback(&path, token)?;
        }
        self.record_account(user, false)?;
        Ok(())
    }

    pub fn select_account(&self, user: &UserDto) -> Result<(), String> {
        self.record_account(user, true)
    }

    fn record_account(&self, user: &UserDto, select: bool) -> Result<(), String> {
        if !valid_account(&user.id) {
            return Err("Invalid account.".into());
        }
        let mut accounts = self.inner.accounts.lock();
        let mut saved = accounts.clone();
        if let Some(old) = saved.users.iter_mut().find(|old| old.id == user.id) {
            *old = user.clone();
        } else {
            saved.users.push(user.clone());
        }
        if select {
            saved.active = Some(user.id.clone());
        }
        self.save_accounts(&saved)?;
        *accounts = saved;
        Ok(())
    }

    pub fn remove_account(&self, account: &str) -> Result<(), String> {
        if !valid_account(account) {
            return Err("Invalid account.".into());
        }
        let key = account_key(account);
        let cleared = delete_keyring(&key).and_then(|_| {
            if read_keyring(&key).is_some() {
                Err("The OS keychain still holds a session token.".into())
            } else {
                Ok(())
            }
        });
        let path = self.inner.directory.join(account);
        if cleared.is_err() {
            // Prevent an inaccessible old keychain entry from restoring after sign-out.
            write_fallback(&path, "")?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("Could not remove the saved sign-in.".into()),
            }
        }
        let mut accounts = self.inner.accounts.lock();
        let mut saved = accounts.clone();
        saved.users.retain(|user| user.id != account);
        if saved.active.as_deref() == Some(account) {
            saved.active = saved.users.first().map(|user| user.id.clone());
        }
        self.save_accounts(&saved)?;
        *accounts = saved;
        Ok(())
    }

    fn save_accounts(&self, accounts: &Accounts) -> Result<(), String> {
        let json =
            serde_json::to_string(accounts).map_err(|_| "Could not save accounts.".to_string())?;
        write_fallback(&self.inner.directory.join("accounts.json"), &json)
    }
}

fn valid_account(id: &str) -> bool {
    id.len() <= 20 && super::snowflake(id)
}
fn account_key(account: &str) -> String {
    format!("discord-{account}")
}

fn write_fallback(path: &Path, token: &str) -> Result<(), String> {
    let save = || -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("missing credential directory"))?;
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        crate::write_file(path, token.as_bytes())
    };
    save().map_err(|_| "Could not save sign-in in the application data directory.".into())
}

pub fn valid_token(token: &str) -> bool {
    (50..=200).contains(&token.len())
        && token
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'_' | b'-'))
}

const SERVICE: &str = "Suncord";

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|err| err.to_string())
}

fn read_keyring(account: &str) -> Option<String> {
    let entry = entry(account).ok()?;
    match entry.get_password() {
        Ok(value) => Some(value),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            eprintln!("credential store unavailable: {err}");
            None
        }
    }
}

fn write_keyring(account: &str, token: &str) -> Result<(), String> {
    entry(account)?
        .set_password(token)
        .map_err(|err| format!("could not store the session in the OS keychain: {err}"))
}

fn delete_keyring(account: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!(
            "could not remove the session from the OS keychain: {err}"
        )),
    }
}
