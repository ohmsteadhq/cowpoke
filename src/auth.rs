//! User accounts (Admin / Operator / Viewer) and login sessions.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use dashmap::DashMap;
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reads everything.
    Viewer,
    /// Viewer + writes tag values and starts/stops services.
    Operator,
    /// Operator + creates and edits services, users and certificates.
    Admin,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UserRec {
    username: String,
    password_hash: String,
    role: Role,
}

#[derive(Serialize, Clone, Debug)]
pub struct User {
    pub username: String,
    pub role: Role,
}

struct Session {
    user: User,
    expires: Instant,
}

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

pub struct Auth {
    path: PathBuf,
    users: Mutex<Vec<UserRec>>,
    sessions: DashMap<String, Session>,
}

impl Auth {
    /// Load `users.json`. With no users, creates `admin` with `COWPOKE_ADMIN_PASSWORD`
    /// or a random password printed to the log once.
    pub fn open(path: &Path) -> Result<Self> {
        let users: Vec<UserRec> = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(path)?)?
        } else {
            vec![]
        };
        let auth = Self {
            path: path.to_path_buf(),
            users: Mutex::new(users),
            sessions: DashMap::new(),
        };
        if auth.users.lock().is_empty() {
            let (pw, generated) = match std::env::var("COWPOKE_ADMIN_PASSWORD") {
                Ok(p) if !p.is_empty() => (p, false),
                _ => (random_token(12), true),
            };
            auth.put_user("admin", Some(&pw), Role::Admin)
                .map_err(anyhow::Error::msg)?;
            if generated {
                tracing::warn!(
                    "created user `admin` with password `{pw}`. Change it after you sign in (or set COWPOKE_ADMIN_PASSWORD before first start)."
                );
            } else {
                tracing::info!("created user `admin` from COWPOKE_ADMIN_PASSWORD");
            }
        }
        Ok(auth)
    }

    pub fn list(&self) -> Vec<User> {
        self.users
            .lock()
            .iter()
            .map(|u| User {
                username: u.username.clone(),
                role: u.role,
            })
            .collect()
    }

    /// Create or update a user. `password: None` keeps the existing password.
    pub fn put_user(&self, username: &str, password: Option<&str>, role: Role) -> Result<(), String> {
        let username = username.trim();
        if username.is_empty() || username.len() > 64 {
            return Err("username must be 1-64 characters".into());
        }
        let mut users = self.users.lock();
        let hash = match password {
            Some(p) if p.len() < 8 => return Err("password must be at least 8 characters".into()),
            Some(p) => Some(hash(p)?),
            None => None,
        };
        match users.iter_mut().find(|u| u.username == username) {
            Some(u) => {
                if u.role == Role::Admin
                    && role != Role::Admin
                    && users_admins(&users) <= 1
                {
                    return Err("can't demote the last admin".into());
                }
                let u = users.iter_mut().find(|u| u.username == username).unwrap();
                u.role = role;
                if let Some(h) = hash {
                    u.password_hash = h;
                }
            }
            None => {
                let Some(h) = hash else {
                    return Err("new users need a password".into());
                };
                users.push(UserRec {
                    username: username.to_string(),
                    password_hash: h,
                    role,
                });
            }
        }
        self.save(&users).map_err(|e| e.to_string())?;
        drop(users);
        // Role changes apply to live sessions immediately.
        for mut s in self.sessions.iter_mut() {
            if s.user.username == username {
                s.user.role = role;
            }
        }
        Ok(())
    }

    pub fn delete_user(&self, username: &str) -> Result<(), String> {
        let mut users = self.users.lock();
        let Some(u) = users.iter().find(|u| u.username == username) else {
            return Err("no such user".into());
        };
        if u.role == Role::Admin && users_admins(&users) <= 1 {
            return Err("can't delete the last admin".into());
        }
        users.retain(|u| u.username != username);
        self.save(&users).map_err(|e| e.to_string())?;
        self.sessions.retain(|_, s| s.user.username != username);
        Ok(())
    }

    /// Check a password and open a session. Returns the session token.
    pub fn login(&self, username: &str, password: &str) -> Option<(String, User)> {
        let users = self.users.lock();
        let rec = users.iter().find(|u| u.username == username);
        let ok = match rec {
            Some(r) => PasswordHash::new(&r.password_hash)
                .ok()
                .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
                .unwrap_or(false),
            None => {
                // Spend the same time on unknown users.
                let _ = hash(password);
                false
            }
        };
        if !ok {
            return None;
        }
        let rec = rec.unwrap();
        let user = User {
            username: rec.username.clone(),
            role: rec.role,
        };
        let token = random_token(32);
        self.sessions.insert(
            token.clone(),
            Session {
                user: user.clone(),
                expires: Instant::now() + SESSION_TTL,
            },
        );
        Some((token, user))
    }

    pub fn session(&self, token: &str) -> Option<User> {
        let s = self.sessions.get(token)?;
        if s.expires < Instant::now() {
            drop(s);
            self.sessions.remove(token);
            return None;
        }
        Some(s.user.clone())
    }

    pub fn logout(&self, token: &str) {
        self.sessions.remove(token);
    }

    fn save(&self, users: &[UserRec]) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(users)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn users_admins(users: &[UserRec]) -> usize {
    users.iter().filter(|u| u.role == Role::Admin).count()
}

fn hash(pw: &str) -> Result<String, String> {
    Argon2::default()
        .hash_password(pw.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

pub fn random_token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill(&mut b[..]);
    hex::encode(b)
}
