//! Projects and their members.

use rusqlite::{OptionalExtension, Row, params};

use super::{Store, StoreResult, now_secs};
use crate::api::{Member, Project, Role};

/// Why the store refused a change to accounts, projects or members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotFound,
    /// No account has this email.
    NoAccount,
    EmailTaken,
    AlreadyMember,
    /// The project would have no owner.
    LastOwner,
    /// The account is the only owner of a project.
    SoleOwner,
    /// The server would have no administrator.
    LastAdmin,
}

const PROJECT_COLUMNS: &str = "p.id, p.name, m.role, p.created_at, p.updated_at";

fn project(r: &Row<'_>) -> rusqlite::Result<Project> {
    let role: String = r.get(2)?;
    Ok(Project {
        id: r.get(0)?,
        name: r.get(1)?,
        role: Role::parse(&role).unwrap_or(Role::Viewer),
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn member(r: &Row<'_>) -> rusqlite::Result<Member> {
    let role: String = r.get(3)?;
    Ok(Member {
        user_id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        role: Role::parse(&role).unwrap_or(Role::Viewer),
    })
}

const MEMBER_QUERY: &str = "SELECT u.id, u.email, u.name, m.role FROM members m
     JOIN users u ON u.id = m.user_id WHERE m.project_id = ?1";

impl Store {
    /// Makes a project with `owner` as its owner and no document, for tests.
    pub fn create_project(&self, id: &str, name: &str, owner: &str) -> StoreResult<()> {
        self.create_project_with(id, name, owner, None, &[])
    }

    /// Makes a project with `owner` as its owner, `first` as the first
    /// entry of its update log, and the assets in `assets`, whose files
    /// [`Store::write_asset_file`] wrote before. It is one transaction: a
    /// crash leaves either the whole project or none of it, and at most
    /// asset files no row refers to, which the next start removes.
    pub fn create_project_with(
        &self,
        id: &str,
        name: &str,
        owner: &str,
        first: Option<&[u8]>,
        assets: &[(String, u64)],
    ) -> StoreResult<()> {
        self.write(|c| {
            let tx = c.transaction()?;
            let now = now_secs();
            tx.execute(
                "INSERT INTO projects (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![id, name, now],
            )?;
            tx.execute(
                "INSERT INTO members (project_id, user_id, role) VALUES (?1, ?2, 'owner')",
                params![id, owner],
            )?;
            for (hash, size) in assets {
                tx.execute(
                    "INSERT OR IGNORE INTO assets (hash, size) VALUES (?1, ?2)",
                    params![hash, i64::try_from(*size).unwrap_or(i64::MAX)],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO project_assets (project_id, hash) VALUES (?1, ?2)",
                    params![id, hash],
                )?;
            }
            if let Some(first) = first {
                tx.execute(
                    "INSERT INTO doc_updates (project_id, seq, bytes) VALUES (?1, 1, ?2)",
                    params![id, first],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        crate::crash::point("project:created");
        Ok(())
    }

    /// The projects `user_id` is a member of, most recently changed first.
    pub fn projects_for(&self, user_id: &str) -> StoreResult<Vec<Project>> {
        self.read(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {PROJECT_COLUMNS} FROM projects p JOIN members m ON m.project_id = p.id
                 WHERE m.user_id = ?1 ORDER BY p.updated_at DESC, p.created_at DESC, p.id"
            ))?;
            let rows = stmt
                .query_map([user_id], project)?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
    }

    /// One project as `user_id` sees it, if they are a member.
    pub fn project_for(&self, project_id: &str, user_id: &str) -> StoreResult<Option<Project>> {
        self.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {PROJECT_COLUMNS} FROM projects p JOIN members m ON m.project_id = p.id
                     WHERE p.id = ?1 AND m.user_id = ?2"
                ),
                params![project_id, user_id],
                project,
            )
            .optional()?)
        })
    }

    pub fn rename_project(&self, project_id: &str, name: &str) -> StoreResult<()> {
        self.write(|c| {
            c.execute(
                "UPDATE projects SET name = ?2, updated_at = ?3 WHERE id = ?1",
                params![project_id, name, now_secs()],
            )?;
            Ok(())
        })
    }

    /// Deletes a project with its document, history and memberships, and
    /// then the asset files no other project holds. Its sync sockets close
    /// with 4004. A crash between the commit and the file deletions leaves
    /// files no row refers to, which [`Store::sweep_assets`] removes at the
    /// next start.
    pub fn delete_project(&self, project_id: &str) -> StoreResult<()> {
        let orphans = self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let held: Vec<String> = {
                let mut stmt =
                    tx.prepare("SELECT hash FROM project_assets WHERE project_id = ?1")?;
                stmt.query_map([project_id], |r| r.get(0))?
                    .collect::<Result<_, _>>()?
            };
            tx.execute("DELETE FROM projects WHERE id = ?1", [project_id])?;
            let mut orphans = Vec::new();
            for hash in held {
                let still: i64 = tx.query_row(
                    "SELECT count(*) FROM project_assets WHERE hash = ?1",
                    [&hash],
                    |r| r.get(0),
                )?;
                if still == 0 {
                    tx.execute("DELETE FROM assets WHERE hash = ?1", [&hash])?;
                    orphans.push(hash);
                }
            }
            tx.commit()?;
            self.grants().project_deleted(project_id);
            Ok(orphans)
        })?;
        crate::crash::point("project:deleted");
        for hash in orphans {
            self.remove_asset_file(&hash);
        }
        // Give the freed pages back to the file system.
        self.write(|c| Ok(c.execute_batch("PRAGMA incremental_vacuum")?))?;
        Ok(())
    }

    /// The members of a project, sorted by name.
    pub fn members(&self, project_id: &str) -> StoreResult<Vec<Member>> {
        self.read(|c| {
            let mut stmt = c.prepare(&format!("{MEMBER_QUERY} ORDER BY u.name, u.email"))?;
            let rows = stmt
                .query_map([project_id], member)?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
    }

    /// `user_id`'s role in a project, if they are a member.
    pub fn role(&self, project_id: &str, user_id: &str) -> StoreResult<Option<Role>> {
        self.read(|c| {
            let role: Option<String> = c
                .query_row(
                    "SELECT role FROM members WHERE project_id = ?1 AND user_id = ?2",
                    params![project_id, user_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(role.as_deref().and_then(Role::parse))
        })
    }

    /// Adds the account with `email` to a project.
    pub fn add_member(
        &self,
        project_id: &str,
        email: &str,
        role: Role,
    ) -> StoreResult<Result<Member, Refusal>> {
        self.write(|c| {
            let tx = c.transaction()?;
            let user: Option<String> = tx
                .query_row("SELECT id FROM users WHERE email = ?1", [email], |r| {
                    r.get(0)
                })
                .optional()?;
            let Some(user) = user else {
                return Ok(Err(Refusal::NoAccount));
            };
            let added = tx.execute(
                "INSERT OR IGNORE INTO members (project_id, user_id, role) VALUES (?1, ?2, ?3)",
                params![project_id, user, role.as_str()],
            )?;
            if added == 0 {
                return Ok(Err(Refusal::AlreadyMember));
            }
            let row = tx.query_row(
                &format!("{MEMBER_QUERY} AND m.user_id = ?2"),
                params![project_id, user],
                member,
            )?;
            tx.commit()?;
            Ok(Ok(row))
        })
    }

    /// Changes a member's role. Their open sync grants follow at once: one
    /// who may no longer write is closed with 4003, one who now may write
    /// can.
    pub fn set_role(
        &self,
        project_id: &str,
        user_id: &str,
        role: Role,
    ) -> StoreResult<Result<Member, Refusal>> {
        self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let Some(old) = current_role(&tx, project_id, user_id)? else {
                return Ok(Err(Refusal::NotFound));
            };
            if old == Role::Owner && role != Role::Owner && owners(&tx, project_id)? <= 1 {
                return Ok(Err(Refusal::LastOwner));
            }
            tx.execute(
                "UPDATE members SET role = ?3 WHERE project_id = ?1 AND user_id = ?2",
                params![project_id, user_id, role.as_str()],
            )?;
            let row = tx.query_row(
                &format!("{MEMBER_QUERY} AND m.user_id = ?2"),
                params![project_id, user_id],
                member,
            )?;
            tx.commit()?;
            self.grants().role_changed(project_id, user_id, role);
            Ok(Ok(row))
        })
    }

    /// Takes `user_id` out of a project. Their sockets to it close with
    /// 4004.
    pub fn remove_member(
        &self,
        project_id: &str,
        user_id: &str,
    ) -> StoreResult<Result<(), Refusal>> {
        self.write(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let Some(old) = current_role(&tx, project_id, user_id)? else {
                return Ok(Err(Refusal::NotFound));
            };
            if old == Role::Owner && owners(&tx, project_id)? <= 1 {
                return Ok(Err(Refusal::LastOwner));
            }
            tx.execute(
                "DELETE FROM members WHERE project_id = ?1 AND user_id = ?2",
                params![project_id, user_id],
            )?;
            tx.commit()?;
            self.grants().member_removed(project_id, user_id);
            Ok(Ok(()))
        })
    }
}

fn current_role(
    c: &rusqlite::Connection,
    project_id: &str,
    user_id: &str,
) -> rusqlite::Result<Option<Role>> {
    let role: Option<String> = c
        .query_row(
            "SELECT role FROM members WHERE project_id = ?1 AND user_id = ?2",
            params![project_id, user_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(role.as_deref().and_then(Role::parse))
}

fn owners(c: &rusqlite::Connection, project_id: &str) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT count(*) FROM members WHERE project_id = ?1 AND role = 'owner'",
        [project_id],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::User;

    #[test]
    fn a_project_keeps_an_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for id in ["a", "b"] {
            store
                .create_account(
                    &User {
                        id: id.into(),
                        email: format!("{id}@example.org"),
                        name: id.into(),
                        is_admin: false,
                    },
                    "x",
                )
                .unwrap()
                .unwrap();
        }
        store.create_project("p", "P", "a").unwrap();
        assert_eq!(
            store.set_role("p", "a", Role::Editor).unwrap(),
            Err(Refusal::LastOwner)
        );
        assert_eq!(
            store.remove_member("p", "a").unwrap(),
            Err(Refusal::LastOwner)
        );
        assert_eq!(
            store
                .add_member("p", "nobody@example.org", Role::Editor)
                .unwrap(),
            Err(Refusal::NoAccount)
        );
        store
            .add_member("p", "b@example.org", Role::Owner)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .add_member("p", "b@example.org", Role::Viewer)
                .unwrap(),
            Err(Refusal::AlreadyMember)
        );
        let a = store.set_role("p", "a", Role::Viewer).unwrap().unwrap();
        assert_eq!(a.role, Role::Viewer);
        assert_eq!(
            store.remove_member("p", "b").unwrap(),
            Err(Refusal::LastOwner)
        );
        assert_eq!(store.remove_member("p", "a").unwrap(), Ok(()));
        assert_eq!(store.members("p").unwrap().len(), 1);
        assert_eq!(
            store.set_role("p", "a", Role::Editor).unwrap(),
            Err(Refusal::NotFound)
        );
    }
}
