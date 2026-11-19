//! Accounts, sessions and first-run setup.

use rusqlite::{Connection, OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use super::{Store, StoreError, StoreResult, hex, now_secs, token_hash};
use crate::access::Grant;
use crate::api::User;
use crate::limits::{SESSION_MAX_AGE, SESSION_RENEW_EVERY, SESSION_TTL};
use crate::store::projects::Refusal;

/// The `settings` key of the SHA-256 of the first-run setup code.
const SETUP_CODE: &str = "setup_code_sha256";

/// What [`Store::create_owner`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Setup {
    Created,
    /// An account exists already.
    Taken,
    /// A setup code is required and this one is missing or wrong.
    WrongCode,
}

/// A session as the store holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
    pub created_at: i64,
    pub expires_at: i64,
}

const USER_COLUMNS: &str = "id, email, name, is_admin";

fn user(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        is_admin: r.get(3)?,
    })
}

fn secs(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// An account id: 64 random bits in hex.
#[must_use]
pub fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl Store {
    pub fn user_count(&self) -> StoreResult<i64> {
        self.read(|c| Ok(c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?))
    }

    /// Makes the first account, the administrator, in one write transaction
    /// that also checks that no account exists and, when a setup code is
    /// stored, that `code` matches it. The code is compared in constant time
    /// and deleted with the same commit, so it works once. `code` is `None`
    /// from `nisaba setup`, which runs on the server's own machine and needs
    /// no code.
    pub fn create_owner(
        &self,
        user: &User,
        password_hash: &str,
        code: Option<Option<&str>>,
    ) -> StoreResult<Setup> {
        self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let refused = match code {
                Some(code) => setup_refused(&tx, code)?,
                None => setup_taken(&tx)?,
            };
            if let Some(refused) = refused {
                return Ok(refused);
            }
            insert_user(&tx, user, Some(password_hash), true)?;
            tx.execute("DELETE FROM settings WHERE key = ?1", [SETUP_CODE])?;
            tx.commit()?;
            Ok(Setup::Created)
        })
    }

    /// Says, before any password work, why setup with `code` would fail:
    /// [`Setup::Taken`] or [`Setup::WrongCode`]. `None` means it may work.
    /// [`Store::create_owner`] checks again in its transaction.
    pub fn setup_refused(&self, code: Option<&str>) -> StoreResult<Option<Setup>> {
        self.read(|c| setup_refused(c, code))
    }

    /// Prepares first-run setup at start. While no account exists and
    /// `needed` is true, makes a new setup code, keeps only its SHA-256 and
    /// returns the code, replacing any code of an earlier start. Otherwise
    /// drops any stored code and returns `None`.
    pub fn prepare_setup(&self, needed: bool) -> StoreResult<Option<String>> {
        let raw: [u8; 16] = rand::random();
        let digits = hex(&raw);
        let code = digits
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-");
        let hash = Sha256::digest(normalize_code(&code).as_bytes()).to_vec();
        self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let users: i64 = tx.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
            tx.execute("DELETE FROM settings WHERE key = ?1", [SETUP_CODE])?;
            let code = if users == 0 && needed {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                    params![SETUP_CODE, hash],
                )?;
                Some(code)
            } else {
                None
            };
            tx.commit()?;
            Ok(code)
        })
    }

    /// Whether first-run setup asks for a code.
    pub fn setup_code_needed(&self) -> StoreResult<bool> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM settings WHERE key = ?1",
                [SETUP_CODE],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    /// The account with this email and its password hash, if any.
    pub fn credentials(&self, email: &str) -> StoreResult<Option<(User, Option<String>)>> {
        self.read(|c| {
            Ok(c.query_row(
                &format!("SELECT {USER_COLUMNS}, password_hash FROM users WHERE email = ?1"),
                [email],
                |r| Ok((user(r)?, r.get(4)?)),
            )
            .optional()?)
        })
    }

    /// The password hash of an account.
    pub fn password_hash(&self, user_id: &str) -> StoreResult<Option<String>> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [user_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
        })
    }

    /// Replaces a password hash with one of the same password made with
    /// newer parameters, unless the hash changed meanwhile.
    pub fn rehash_password(&self, user_id: &str, old: &str, new: &str) -> StoreResult<bool> {
        self.write(|c| {
            Ok(c.execute(
                "UPDATE users SET password_hash = ?3 WHERE id = ?1 AND password_hash = ?2",
                params![user_id, old, new],
            )? == 1)
        })
    }

    /// Sets a new password and ends every session of the account but
    /// `keep`. Their sync sockets close with 4401.
    pub fn change_password(&self, user_id: &str, hash: &str, keep: &str) -> StoreResult<()> {
        let keep = token_hash(keep);
        self.write(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE users SET password_hash = ?2 WHERE id = ?1",
                params![user_id, hash],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE user_id = ?1 AND token_hash != ?2",
                params![user_id, keep],
            )?;
            tx.commit()?;
            self.grants()
                .signed_out_except(user_id, &keep, "your password was changed");
            Ok(())
        })
    }

    /// Makes an account. `Err(EmailTaken)` when another account has the
    /// email.
    pub fn create_account(
        &self,
        user: &User,
        password_hash: &str,
    ) -> StoreResult<Result<(), Refusal>> {
        self.write(
            |c| match insert_user(c, user, Some(password_hash), user.is_admin) {
                Ok(()) => Ok(Ok(())),
                Err(rusqlite::Error::SqliteFailure(f, _))
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    Ok(Err(Refusal::EmailTaken))
                }
                Err(e) => Err(e.into()),
            },
        )
    }

    /// Changes the display name.
    pub fn rename_account(&self, user_id: &str, name: &str) -> StoreResult<Option<User>> {
        self.write(|c| {
            c.execute(
                "UPDATE users SET name = ?2 WHERE id = ?1",
                params![user_id, name],
            )?;
            Ok(c.query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"),
                [user_id],
                user,
            )
            .optional()?)
        })
    }

    /// Every account, sorted by email.
    pub fn accounts(&self) -> StoreResult<Vec<User>> {
        self.read(|c| {
            let mut stmt =
                c.prepare(&format!("SELECT {USER_COLUMNS} FROM users ORDER BY email"))?;
            let users = stmt.query_map([], user)?.collect::<Result<_, _>>()?;
            Ok(users)
        })
    }

    /// Deletes an account with its sessions and memberships. Its sync
    /// sockets close with 4401. Refused while it is the only owner of a
    /// project, or the last administrator.
    pub fn delete_account(&self, user_id: &str) -> StoreResult<Result<(), Refusal>> {
        self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let is_admin: Option<bool> = tx
                .query_row("SELECT is_admin FROM users WHERE id = ?1", [user_id], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(is_admin) = is_admin else {
                return Ok(Err(Refusal::NotFound));
            };
            let sole: i64 = tx.query_row(
                "SELECT count(*) FROM members m WHERE m.user_id = ?1 AND m.role = 'owner'
                 AND NOT EXISTS (SELECT 1 FROM members o WHERE o.project_id = m.project_id
                                 AND o.role = 'owner' AND o.user_id != ?1)",
                [user_id],
                |r| r.get(0),
            )?;
            if sole > 0 {
                return Ok(Err(Refusal::SoleOwner));
            }
            if is_admin {
                let admins: i64 =
                    tx.query_row("SELECT count(*) FROM users WHERE is_admin = 1", [], |r| {
                        r.get(0)
                    })?;
                if admins <= 1 {
                    return Ok(Err(Refusal::LastAdmin));
                }
            }
            tx.execute("DELETE FROM users WHERE id = ?1", [user_id])?;
            tx.commit()?;
            self.grants().user_removed(user_id);
            Ok(Ok(()))
        })
    }

    /// Adds a session and drops every expired one. Returns when it ends.
    pub fn create_session(&self, token: &str, user_id: &str) -> StoreResult<SessionInfo> {
        self.create_session_for(token, user_id, secs(SESSION_TTL))
    }

    /// [`Store::create_session`] with a lifetime of `ttl` seconds, for tests
    /// of expiry.
    pub fn create_session_for(
        &self,
        token: &str,
        user_id: &str,
        ttl: i64,
    ) -> StoreResult<SessionInfo> {
        let hash = token_hash(token);
        self.write(|c| {
            let now = now_secs();
            c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
            let info = SessionInfo {
                created_at: now,
                expires_at: now.saturating_add(ttl),
            };
            c.execute(
                "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![hash, user_id, info.created_at, info.expires_at],
            )?;
            Ok(info)
        })
    }

    /// The account of a session that has not expired.
    pub fn session(&self, token: &str) -> StoreResult<Option<(User, SessionInfo)>> {
        let hash = token_hash(token);
        self.read(|c| {
            Ok(c.query_row(
                "SELECT u.id, u.email, u.name, u.is_admin, s.created_at, s.expires_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                params![hash, now_secs()],
                |r| {
                    Ok((
                        user(r)?,
                        SessionInfo {
                            created_at: r.get(4)?,
                            expires_at: r.get(5)?,
                        },
                    ))
                },
            )
            .optional()?)
        })
    }

    /// Moves a session's end to 30 days from now when it was last renewed
    /// more than a day ago, never beyond 90 days after sign-in. Returns the
    /// new end when it moved. Open sync sockets of the session keep going
    /// until the new end.
    pub fn renew_session(&self, token: &str, info: SessionInfo) -> StoreResult<Option<i64>> {
        let now = now_secs();
        let renewed = info.expires_at - secs(SESSION_TTL);
        let end = (now + secs(SESSION_TTL)).min(info.created_at + secs(SESSION_MAX_AGE));
        if now - renewed < secs(SESSION_RENEW_EVERY) || end <= info.expires_at {
            return Ok(None);
        }
        let hash = token_hash(token);
        self.write(|c| {
            let changed = c.execute(
                "UPDATE sessions SET expires_at = ?2 WHERE token_hash = ?1 AND expires_at > ?3",
                params![hash, end, now],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            self.grants().renewed(&hash, end);
            Ok(Some(end))
        })
    }

    /// Signs a session out. Its sync sockets close with 4401.
    pub fn delete_session(&self, token: &str) -> StoreResult<()> {
        let hash = token_hash(token);
        self.write(|c| {
            c.execute("DELETE FROM sessions WHERE token_hash = ?1", [&hash])?;
            self.grants().signed_out(&hash);
            Ok(())
        })
    }

    /// Signs out every session of an account. Their sync sockets close with
    /// 4401.
    pub fn delete_sessions(&self, user_id: &str) -> StoreResult<()> {
        self.write(|c| {
            c.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
            self.grants()
                .signed_out_except(user_id, &[], "signed out everywhere");
            Ok(())
        })
    }

    /// Lets the session `token` act in `project`, for a sync socket or for
    /// one request that changes the document. Refuses with 4401 when the
    /// session is missing or expired, and with 4004 when the user is not a
    /// member. The grant is registered under the write lock, so a sign-out
    /// or removal either happens before (and the grant is refused) or after
    /// (and ends it).
    pub fn grant(&self, token: &str, project: &str) -> StoreResult<Result<Grant, u16>> {
        let hash = token_hash(token);
        self.write(|c| {
            let found = c
                .query_row(
                    "SELECT u.id, u.email, u.name, u.is_admin, s.expires_at, m.role
                     FROM sessions s JOIN users u ON u.id = s.user_id
                     LEFT JOIN members m ON m.user_id = u.id AND m.project_id = ?3
                     WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                    params![hash, now_secs(), project],
                    |r| {
                        Ok((
                            user(r)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((user, expires_at, role)) = found else {
                return Ok(Err(nisaba_sync::close::SIGNED_OUT));
            };
            let Some(role) = role.as_deref().and_then(crate::api::Role::parse) else {
                return Ok(Err(nisaba_sync::close::NOT_FOUND));
            };
            Ok(Ok(self
                .inner
                .grants
                .add(hash, user, project, role, expires_at)))
        })
    }
}

pub(super) fn insert_user(
    c: &Connection,
    user: &User,
    hash: Option<&str>,
    is_admin: bool,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users (id, email, name, password_hash, is_admin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![user.id, user.email, user.name, hash, is_admin, now_secs()],
    )
    .map(drop)
}

fn setup_taken(c: &Connection) -> StoreResult<Option<Setup>> {
    let users: i64 = c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
    Ok((users > 0).then_some(Setup::Taken))
}

/// Why setup with `code` cannot work now, if it cannot. The code is compared
/// by SHA-256 in constant time.
fn setup_refused(c: &Connection, code: Option<&str>) -> StoreResult<Option<Setup>> {
    if let Some(taken) = setup_taken(c)? {
        return Ok(Some(taken));
    }
    let stored: Option<Vec<u8>> = c
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [SETUP_CODE],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(stored) = stored {
        use subtle::ConstantTimeEq as _;
        let given = Sha256::digest(normalize_code(code.unwrap_or("")).as_bytes());
        if !bool::from(given.as_slice().ct_eq(&stored)) {
            return Ok(Some(Setup::WrongCode));
        }
    }
    Ok(None)
}

/// A setup code as typed: hex digits only, lower case. People may copy it
/// with or without the dashes and in either case.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

impl From<StoreError> for String {
    fn from(e: StoreError) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn person(id: &str, admin: bool) -> User {
        User {
            id: id.into(),
            email: format!("{id}@example.org"),
            name: id.into(),
            is_admin: admin,
        }
    }

    #[test]
    fn a_session_is_renewed_once_a_day_and_never_past_90_days() {
        let (_dir, store) = store();
        store
            .create_account(&person("u", false), "x")
            .unwrap()
            .unwrap();
        let info = store.create_session("t", "u").unwrap();
        assert_eq!(store.renew_session("t", info).unwrap(), None, "just made");
        let day = secs(SESSION_RENEW_EVERY);
        let ttl = secs(SESSION_TTL);
        // As if it was signed in and last renewed two days ago.
        let old = SessionInfo {
            created_at: info.created_at - 2 * day,
            expires_at: info.expires_at - 2 * day,
        };
        let end = store.renew_session("t", old).unwrap().expect("renewed");
        assert!((end - (now_secs() + ttl)).abs() <= 1);
        // Signed in 89 days ago: the end stops at 90 days.
        let ancient = SessionInfo {
            created_at: now_secs() - 89 * day,
            expires_at: now_secs() + day / 2,
        };
        let end = store.renew_session("t", ancient).unwrap().expect("renewed");
        assert_eq!(end, ancient.created_at + secs(SESSION_MAX_AGE));
        let past = SessionInfo {
            created_at: now_secs() - 90 * day,
            expires_at: now_secs() + 10,
        };
        assert_eq!(store.renew_session("t", past).unwrap(), None);
    }

    #[test]
    fn the_last_administrator_and_a_sole_owner_stay() {
        let (_dir, store) = store();
        store
            .create_account(&person("a", true), "x")
            .unwrap()
            .unwrap();
        store
            .create_account(&person("b", false), "x")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.create_account(&person("b", false), "x").unwrap(),
            Err(Refusal::EmailTaken)
        );
        assert_eq!(store.delete_account("a").unwrap(), Err(Refusal::LastAdmin));
        store.create_project("p", "P", "b").unwrap();
        assert_eq!(store.delete_account("b").unwrap(), Err(Refusal::SoleOwner));
        assert_eq!(
            store.delete_account("nobody").unwrap(),
            Err(Refusal::NotFound)
        );
        store.delete_project("p").unwrap();
        assert_eq!(store.delete_account("b").unwrap(), Ok(()));
        assert_eq!(store.accounts().unwrap().len(), 1);
    }
}
