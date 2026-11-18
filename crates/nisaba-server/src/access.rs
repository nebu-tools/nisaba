//! Who holds a sync socket open, in which role, and the signal that closes
//! it.
//!
//! A socket belongs to the session that opened it. [`crate::store::Store`]
//! hands out a [`Grant`] for it, and registers the grant here while it holds
//! the write connection. Every store method that changes access ends or
//! changes the matching grants under the same lock:
//!
//! | Change | Grants | Close code |
//! |---|---|---|
//! | sign-out, sign-out everywhere, password change elsewhere | that session's, or the account's others | 4401 |
//! | account deleted | the account's | 4401 |
//! | session expired | that session's, within a minute | 4401 |
//! | member removed, project deleted | that member's, or the project's | 4004 |
//! | role changed so the member may no longer write | that member's in the project | 4003 |
//! | role changed otherwise | the role, at once, for the next update | none |
//! | session renewed | the end moves | none |
//!
//! So no grant outlives the access it was made from, and no route has to
//! remember to close sockets. The room asks a grant's [`Grant::access`]
//! under the store's lock as it writes, so an update that waited while
//! access changed follows the change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nisaba_sync::close;
use tokio::sync::watch;

use crate::api::{Role, User};
use crate::store::now_secs;

/// Why a grant ended: the close code and a reason for the close frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct End {
    pub code: u16,
    pub reason: &'static str,
}

impl End {
    const EXPIRED: Self = Self {
        code: close::SIGNED_OUT,
        reason: "your session expired; sign in again",
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    role: Role,
    expires_at: i64,
    ended: Option<End>,
}

impl State {
    fn end(&self) -> Option<End> {
        self.ended
            .or_else(|| (now_secs() >= self.expires_at).then_some(End::EXPIRED))
    }
}

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
    state: watch::Sender<State>,
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
        let (state, watch) = watch::channel(State {
            role,
            expires_at,
            ended: None,
        });
        self.lock().insert(
            id,
            Holder {
                session,
                user: user.id.clone(),
                project: project.to_owned(),
                state,
            },
        );
        Grant {
            user,
            project: project.to_owned(),
            state: watch,
            id,
            grants: Arc::clone(self),
        }
    }

    /// Ends every grant of the session whose token hashes to `session`.
    pub(crate) fn signed_out(&self, session: &[u8]) {
        self.end(
            |h| h.session == session,
            End {
                code: close::SIGNED_OUT,
                reason: "signed out; sign in again",
            },
        );
    }

    /// Ends every grant of `user` but those of the session `keep`.
    pub(crate) fn signed_out_except(&self, user: &str, keep: &[u8], reason: &'static str) {
        self.end(
            |h| h.user == user && h.session != keep,
            End {
                code: close::SIGNED_OUT,
                reason,
            },
        );
    }

    /// Ends every grant of `user`.
    pub(crate) fn user_removed(&self, user: &str) {
        self.end(
            |h| h.user == user,
            End {
                code: close::SIGNED_OUT,
                reason: "your account was deleted",
            },
        );
    }

    /// Ends every grant of `user` in `project`.
    pub(crate) fn member_removed(&self, project: &str, user: &str) {
        self.end(
            |h| h.user == user && h.project == project,
            End {
                code: close::NOT_FOUND,
                reason: "you are no longer a member of this project",
            },
        );
    }

    /// Ends every grant in `project`.
    pub(crate) fn project_deleted(&self, project: &str) {
        self.end(
            |h| h.project == project,
            End {
                code: close::NOT_FOUND,
                reason: "this project was deleted",
            },
        );
    }

    /// Gives `user`'s grants in `project` their new role. A grant that could
    /// write and no longer can ends with 4003.
    pub(crate) fn role_changed(&self, project: &str, user: &str, role: Role) {
        for holder in self
            .lock()
            .values()
            .filter(|h| h.user == user && h.project == project)
        {
            holder.state.send_if_modified(|state| {
                if state.ended.is_some() {
                    return false;
                }
                if state.role.writes() && !role.writes() {
                    state.ended = Some(End {
                        code: close::FORBIDDEN,
                        reason: "your role in this project changed; you may no longer edit it",
                    });
                }
                state.role = role;
                true
            });
        }
    }

    /// Moves the end of every grant of a session.
    pub(crate) fn renewed(&self, session: &[u8], expires_at: i64) {
        for holder in self.lock().values().filter(|h| h.session == session) {
            holder
                .state
                .send_modify(|state| state.expires_at = expires_at);
        }
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

    fn end(&self, which: impl Fn(&Holder) -> bool, end: End) {
        for holder in self.lock().values().filter(|h| which(h)) {
            holder.state.send_if_modified(|state| {
                let first = state.ended.is_none();
                state.ended.get_or_insert(end);
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

/// The right of one socket, or one request, to act in a project.
pub struct Grant {
    pub user: User,
    pub project: String,
    state: watch::Receiver<State>,
    id: u64,
    grants: Arc<Grants>,
}

impl Grant {
    /// Why the grant ended, if it has.
    #[must_use]
    pub fn ended(&self) -> Option<End> {
        self.state.borrow().end()
    }

    /// The role now.
    #[must_use]
    pub fn role(&self) -> Role {
        self.state.borrow().role
    }

    /// The role as the room asks for it: `None` once the grant ended.
    #[must_use]
    pub fn access(&self) -> nisaba_sync::Access {
        let state = self.state.clone();
        Arc::new(move || {
            let state = state.borrow();
            state.end().is_none().then(|| state.role.sync())
        })
    }

    /// A handle that waits for the end, apart from the grant itself.
    #[must_use]
    pub fn ending(&self) -> Ending {
        Ending {
            state: self.state.clone(),
        }
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.grants.lock().remove(&self.id);
    }
}

pub struct Ending {
    state: watch::Receiver<State>,
}

impl Ending {
    /// Resolves when the grant ends. Cancel-safe.
    pub async fn wait(&mut self) -> End {
        loop {
            let state = *self.state.borrow_and_update();
            if let Some(end) = state.end() {
                return end;
            }
            // Wake at least every minute: the monotonic clock may stand still
            // while the machine sleeps, and the session expires by the wall
            // clock.
            let left = state.expires_at - now_secs();
            let nap = Duration::from_secs(u64::try_from(left.clamp(1, 60)).unwrap_or(60));
            tokio::select! {
                changed = self.state.changed() => {
                    if changed.is_err() {
                        return End::EXPIRED;
                    }
                }
                () = tokio::time::sleep(nap) => {}
            }
        }
    }
}
