//! Who holds a sync socket open, and the signal that closes it.
//!
//! A socket belongs to the session that opened it. [`crate::store::Store`]
//! hands out a [`Grant`] for it, and registers the grant here while it holds
//! the database lock. Every store method that takes access away (signing
//! out, removing a member, deleting an account) ends the matching grants
//! under the same lock. So no grant outlives the access it was made from,
//! and no route has to remember to close sockets.
//!
//! A grant also ends when its session expires. The socket loop waits on
//! [`Ending::wait`] and asks [`Grant::ended`] before it applies each frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nisaba_sync::{Role, close};
use tokio::sync::watch;

use crate::api::User;
use crate::store::now_secs;

/// The grants that are open.
#[derive(Default)]
pub struct Grants {
    next: AtomicU64,
    open: Mutex<HashMap<u64, Holder>>,
}

struct Holder {
    session: Vec<u8>,
    user: String,
    project: String,
    end: watch::Sender<Option<u16>>,
}

impl Grants {
    pub(crate) fn add(
        self: &Arc<Self>,
        session: Vec<u8>,
        user: User,
        project: &str,
        role: Role,
        expires_at: i64,
    ) -> Grant {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (end, ended) = watch::channel(None);
        self.lock().insert(
            id,
            Holder {
                session,
                user: user.id.clone(),
                project: project.to_owned(),
                end,
            },
        );
        Grant {
            user,
            role,
            expires_at,
            ended,
            id,
            grants: Arc::clone(self),
        }
    }

    /// Ends every grant of the session whose token hashes to `session`.
    pub(crate) fn signed_out(&self, session: &[u8]) {
        self.end(|h| h.session == session, close::SIGNED_OUT);
    }

    /// Ends every grant of `user`.
    pub(crate) fn user_removed(&self, user: &str) {
        self.end(|h| h.user == user, close::SIGNED_OUT);
    }

    /// Ends every grant of `user` in `project`.
    pub(crate) fn member_removed(&self, project: &str, user: &str) {
        self.end(|h| h.user == user && h.project == project, close::NOT_FOUND);
    }

    /// How many grants are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn end(&self, which: impl Fn(&Holder) -> bool, code: u16) {
        for holder in self.lock().values().filter(|h| which(h)) {
            holder.end.send_if_modified(|c| {
                let first = c.is_none();
                c.get_or_insert(code);
                first
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Holder>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The right of one socket to stay open.
pub struct Grant {
    pub user: User,
    pub role: Role,
    expires_at: i64,
    ended: watch::Receiver<Option<u16>>,
    id: u64,
    grants: Arc<Grants>,
}

impl Grant {
    /// The close code if the grant has ended: 4401 when the session ended or
    /// expired, 4004 when the user lost access to the project.
    #[must_use]
    pub fn ended(&self) -> Option<u16> {
        if let Some(code) = *self.ended.borrow() {
            return Some(code);
        }
        (now_secs() >= self.expires_at).then_some(close::SIGNED_OUT)
    }

    /// Whether the grant still holds, for the room to ask when it writes.
    #[must_use]
    pub fn admits(&self) -> nisaba_sync::Admit {
        let ended = self.ended.clone();
        let expires_at = self.expires_at;
        std::sync::Arc::new(move || ended.borrow().is_none() && now_secs() < expires_at)
    }

    /// A handle that waits for the end, apart from the grant itself.
    #[must_use]
    pub fn ending(&self) -> Ending {
        Ending {
            expires_at: self.expires_at,
            ended: self.ended.clone(),
        }
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.grants.lock().remove(&self.id);
    }
}

pub struct Ending {
    expires_at: i64,
    ended: watch::Receiver<Option<u16>>,
}

impl Ending {
    /// Resolves with the close code when the grant ends. Cancel-safe.
    pub async fn wait(&mut self) -> u16 {
        loop {
            if let Some(code) = *self.ended.borrow_and_update() {
                return code;
            }
            let left = self.expires_at - now_secs();
            if left <= 0 {
                return close::SIGNED_OUT;
            }
            // Wake at least every minute: the monotonic clock may stand still
            // while the machine sleeps, and the session expires by the wall
            // clock.
            let nap = Duration::from_secs(u64::try_from(left.min(60)).unwrap_or(60));
            tokio::select! {
                changed = self.ended.changed() => {
                    if changed.is_err() {
                        return close::SIGNED_OUT;
                    }
                }
                () = tokio::time::sleep(nap) => {}
            }
        }
    }
}
