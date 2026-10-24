use std::sync::OnceLock;

use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::error::{SiteError, SiteResult};
use crate::security::{
    hash_password, hash_secret, random_secret, validate_password, verify_password,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            "member" => Self::Member,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Owner => 3,
            Self::Admin => 2,
            Self::Member => 1,
        }
    }

    pub fn can_manage_members(self) -> bool {
        self == Self::Owner
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub email_verified: bool,
    pub is_platform_admin: bool,
    pub disabled: bool,
    pub created_ms: i64,
}

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub session_id: String,
    pub user_id: String,
    pub organization_id: String,
    pub organization_name: String,
    pub email: String,
    pub display_name: String,
    pub email_verified: bool,
    pub is_platform_admin: bool,
    pub role: Role,
    pub csrf: String,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    pub expires_ms: i64,
}

impl AuthSession {
    pub fn require_verified(&self) -> SiteResult<()> {
        if self.email_verified {
            Ok(())
        } else {
            Err(SiteError::Forbidden)
        }
    }

    pub fn require_role(&self, role: Role) -> SiteResult<()> {
        if self.role.rank() >= role.rank() {
            Ok(())
        } else {
            Err(SiteError::Forbidden)
        }
    }

    pub fn require_platform_admin(&self) -> SiteResult<()> {
        if self.is_platform_admin && self.email_verified {
            Ok(())
        } else {
            Err(SiteError::NotFound)
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewSession {
    pub id: String,
    pub secret: String,
    pub csrf: String,
    pub expires_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Signup {
    pub user: User,
    pub organization_id: String,
    pub organization_name: String,
    pub session: NewSession,
    pub verification_secret: String,
}

#[allow(clippy::large_enum_variant)]
pub enum SignupOutcome {
    Existing,
    Created(Box<Signup>),
}

pub struct NewAccount<'a> {
    pub email: &'a str,
    pub display_name: &'a str,
    pub password: &'a str,
    pub now_ms: i64,
    pub session_ttl_ms: i64,
    pub is_admin: bool,
    pub client_tag: &'a str,
}

pub fn create_user_and_org(
    conn: &rusqlite::Connection,
    request: NewAccount<'_>,
) -> SiteResult<SignupOutcome> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE email = ?1",
            params![request.email],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        verify_dummy(request.password);
        return Ok(SignupOutcome::Existing);
    }
    let password_hash = hash_password(request.password)?;
    let user_id = format!("usr_{}", Uuid::new_v4());
    let org_id = format!("org_{}", Uuid::new_v4());
    let org_name = personal_org_name(request.display_name);
    let slug = unique_slug(conn, &org_name)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO users (id,email,display_name,password_hash,email_verified,is_platform_admin,disabled,created_ms,updated_ms) VALUES (?1,?2,?3,?4,0,?5,0,?6,?6)",
        params![
            user_id,
            request.email,
            request.display_name,
            password_hash,
            i64::from(request.is_admin),
            request.now_ms
        ],
    )?;
    tx.execute(
        "INSERT INTO organizations (id,name,slug,created_ms) VALUES (?1,?2,?3,?4)",
        params![org_id, org_name, slug, request.now_ms],
    )?;
    tx.execute(
        "INSERT INTO memberships (user_id,organization_id,role,created_ms) VALUES (?1,?2,'owner',?3)",
        params![user_id, org_id, request.now_ms],
    )?;
    let session = insert_session(
        &tx,
        &user_id,
        &org_id,
        request.now_ms,
        request.session_ttl_ms,
        request.client_tag,
    )?;
    let verification_secret = insert_email_event(
        &tx,
        &user_id,
        request.email,
        "verify_email",
        request.now_ms,
        24 * 60 * 60 * 1000,
    )?;
    tx.commit()?;
    Ok(SignupOutcome::Created(Box::new(Signup {
        user: User {
            id: user_id,
            email: request.email.to_string(),
            display_name: request.display_name.to_string(),
            email_verified: false,
            is_platform_admin: request.is_admin,
            disabled: false,
            created_ms: request.now_ms,
        },
        organization_id: org_id,
        organization_name: org_name,
        session,
        verification_secret,
    })))
}

fn personal_org_name(display_name: &str) -> String {
    let first = display_name.split_whitespace().next().unwrap_or("Personal");
    format!("{first}'s workspace")
}

fn unique_slug(conn: &rusqlite::Connection, name: &str) -> SiteResult<String> {
    let base = crate::security::slugify(name);
    for attempt in 0..8 {
        let candidate = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{}", &Uuid::new_v4().simple().to_string()[..6])
        };
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM organizations WHERE slug = ?1",
                params![candidate],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(candidate);
        }
    }
    Err(SiteError::Conflict(
        "Could not allocate a unique workspace slug.",
    ))
}

pub fn insert_session(
    conn: &rusqlite::Connection,
    user_id: &str,
    org_id: &str,
    now_ms: i64,
    ttl_ms: i64,
    client_tag: &str,
) -> SiteResult<NewSession> {
    conn.execute(
        "DELETE FROM sessions WHERE expires_ms < ?1 OR (revoked_ms IS NOT NULL AND revoked_ms < ?1)",
        params![now_ms.saturating_sub(90 * 24 * 60 * 60 * 1000)],
    )?;
    let id = format!("ses_{}", Uuid::new_v4());
    let secret = random_secret();
    let csrf = random_secret();
    let expires_ms = now_ms.saturating_add(ttl_ms);
    conn.execute(
        "INSERT INTO sessions (id,user_id,organization_id,secret_hash,csrf,created_ms,last_seen_ms,expires_ms,revoked_ms,client_tag) VALUES (?1,?2,?3,?4,?5,?6,?6,?7,NULL,?8)",
        params![id, user_id, org_id, hash_secret(&secret), csrf, now_ms, expires_ms, client_tag],
    )?;
    Ok(NewSession {
        id,
        secret,
        csrf,
        expires_ms,
    })
}

pub fn load_session(
    conn: &rusqlite::Connection,
    raw_secret: &str,
    now_ms: i64,
    idle_ttl_ms: i64,
) -> SiteResult<Option<AuthSession>> {
    if raw_secret.len() != 43 {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT s.id,s.user_id,s.organization_id,s.csrf,s.created_ms,s.last_seen_ms,s.expires_ms,\
                    u.email,u.display_name,u.email_verified,u.is_platform_admin,u.disabled,o.name,m.role \
             FROM sessions s JOIN users u ON u.id=s.user_id \
             JOIN organizations o ON o.id=s.organization_id \
             JOIN memberships m ON m.user_id=s.user_id AND m.organization_id=s.organization_id \
             WHERE s.secret_hash=?1 AND s.revoked_ms IS NULL AND s.expires_ms>?2",
            params![hash_secret(raw_secret), now_ms],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)? != 0,
                    row.get::<_, i64>(10)? != 0,
                    row.get::<_, i64>(11)? != 0,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                ))
            },
        )
        .optional()?;
    let Some((
        session_id,
        user_id,
        organization_id,
        csrf,
        created_ms,
        last_seen_ms,
        expires_ms,
        email,
        display_name,
        email_verified,
        is_platform_admin,
        disabled,
        organization_name,
        role_raw,
    )) = row
    else {
        return Ok(None);
    };
    let Some(role) = Role::parse(&role_raw) else {
        return Ok(None);
    };
    if disabled || now_ms.saturating_sub(last_seen_ms) > idle_ttl_ms {
        revoke_session(conn, &session_id, now_ms)?;
        return Ok(None);
    }
    if now_ms.saturating_sub(last_seen_ms) > 300_000 {
        conn.execute(
            "UPDATE sessions SET last_seen_ms=?1 WHERE id=?2",
            params![now_ms, session_id],
        )?;
    }
    Ok(Some(AuthSession {
        session_id,
        user_id,
        organization_id,
        organization_name,
        email,
        display_name,
        email_verified,
        is_platform_admin,
        role,
        csrf,
        created_ms,
        last_seen_ms: now_ms.max(last_seen_ms),
        expires_ms,
    }))
}

pub fn authenticate(
    conn: &rusqlite::Connection,
    email: &str,
    password: &str,
) -> SiteResult<Option<User>> {
    let row = conn
        .query_row(
            "SELECT id,email,display_name,password_hash,email_verified,is_platform_admin,disabled,created_ms FROM users WHERE email=?1",
            params![email],
            |row| {
                Ok((
                    User {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        display_name: row.get(2)?,
                        email_verified: row.get::<_, i64>(4)? != 0,
                        is_platform_admin: row.get::<_, i64>(5)? != 0,
                        disabled: row.get::<_, i64>(6)? != 0,
                        created_ms: row.get(7)?,
                    },
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((user, stored_hash)) = row else {
        verify_dummy(password);
        return Ok(None);
    };
    if user.disabled {
        verify_dummy(password);
        return Ok(None);
    }
    if !verify_password(&stored_hash, password) {
        return Ok(None);
    }
    Ok(Some(user))
}

fn verify_dummy(password: &str) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let hash = DUMMY.get_or_init(|| {
        hash_password("dummy password for timing equalization").unwrap_or_default()
    });
    let _ = verify_password(hash, password);
}

pub fn revoke_session(
    conn: &rusqlite::Connection,
    session_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE sessions SET revoked_ms=?1 WHERE id=?2 AND revoked_ms IS NULL",
        params![now_ms, session_id],
    )?;
    Ok(())
}

pub fn revoke_user_sessions(
    conn: &rusqlite::Connection,
    user_id: &str,
    now_ms: i64,
    except: Option<&str>,
) -> SiteResult<()> {
    match except {
        Some(session_id) => {
            conn.execute(
                "UPDATE sessions SET revoked_ms=?1 WHERE user_id=?2 AND id<>?3 AND revoked_ms IS NULL",
                params![now_ms, user_id, session_id],
            )?;
        }
        None => {
            conn.execute(
                "UPDATE sessions SET revoked_ms=?1 WHERE user_id=?2 AND revoked_ms IS NULL",
                params![now_ms, user_id],
            )?;
        }
    }
    Ok(())
}

pub struct SessionRow {
    pub id: String,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    pub expires_ms: i64,
    pub client_tag: String,
}

pub fn sessions_for_user(
    conn: &rusqlite::Connection,
    user_id: &str,
    now_ms: i64,
) -> SiteResult<Vec<SessionRow>> {
    let mut statement = conn.prepare(
        "SELECT id,created_ms,last_seen_ms,expires_ms,client_tag FROM sessions WHERE user_id=?1 AND revoked_ms IS NULL AND expires_ms>?2 ORDER BY last_seen_ms DESC LIMIT 50",
    )?;
    let rows = statement
        .query_map(params![user_id, now_ms], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                created_ms: row.get(1)?,
                last_seen_ms: row.get(2)?,
                expires_ms: row.get(3)?,
                client_tag: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn insert_email_event(
    conn: &rusqlite::Connection,
    user_id: &str,
    email: &str,
    kind: &str,
    now_ms: i64,
    ttl_ms: i64,
) -> SiteResult<String> {
    conn.execute(
        "DELETE FROM email_events WHERE (used_ms IS NOT NULL AND used_ms < ?1) OR expires_ms < ?1",
        params![now_ms.saturating_sub(30 * 24 * 60 * 60 * 1000)],
    )?;
    let secret = random_secret();
    conn.execute(
        "INSERT INTO email_events (id,user_id,kind,secret_hash,email,created_ms,expires_ms,used_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,NULL)",
        params![
            format!("emv_{}", Uuid::new_v4()),
            user_id,
            kind,
            hash_secret(&secret),
            email,
            now_ms,
            now_ms.saturating_add(ttl_ms)
        ],
    )?;
    Ok(secret)
}

pub fn email_event_user(
    conn: &rusqlite::Connection,
    raw_secret: &str,
    kind: &str,
    now_ms: i64,
) -> SiteResult<Option<(String, String)>> {
    if raw_secret.len() != 43 {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT user_id,email FROM email_events WHERE secret_hash=?1 AND kind=?2 AND used_ms IS NULL AND expires_ms>?3",
            params![hash_secret(raw_secret), kind, now_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row)
}

pub fn verify_email(
    conn: &rusqlite::Connection,
    raw_secret: &str,
    now_ms: i64,
) -> SiteResult<bool> {
    let Some((user_id, _email)) = email_event_user(conn, raw_secret, "verify_email", now_ms)?
    else {
        return Ok(false);
    };
    let hash = hash_secret(raw_secret);
    let tx = conn.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE email_events SET used_ms=?1 WHERE secret_hash=?2 AND used_ms IS NULL",
        params![now_ms, hash],
    )?;
    if changed != 1 {
        return Ok(false);
    }
    tx.execute(
        "UPDATE users SET email_verified=1,updated_ms=?1 WHERE id=?2",
        params![now_ms, user_id],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn reset_password(
    conn: &rusqlite::Connection,
    raw_secret: &str,
    new_password: &str,
    now_ms: i64,
) -> SiteResult<bool> {
    let Some((user_id, _email)) = email_event_user(conn, raw_secret, "reset_password", now_ms)?
    else {
        return Ok(false);
    };
    validate_password(new_password)?;
    let password_hash = hash_password(new_password)?;
    let hash = hash_secret(raw_secret);
    let tx = conn.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE email_events SET used_ms=?1 WHERE secret_hash=?2 AND used_ms IS NULL",
        params![now_ms, hash],
    )?;
    if changed != 1 {
        return Ok(false);
    }
    tx.execute(
        "UPDATE users SET password_hash=?1,updated_ms=?2 WHERE id=?3",
        params![password_hash, now_ms, user_id],
    )?;
    tx.execute(
        "UPDATE sessions SET revoked_ms=?1 WHERE user_id=?2 AND revoked_ms IS NULL",
        params![now_ms, user_id],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn set_email_verified_for_tests(conn: &rusqlite::Connection, user_id: &str) -> SiteResult<()> {
    conn.execute(
        "UPDATE users SET email_verified=1 WHERE id=?1",
        params![user_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MemberRow {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    pub role: Role,
    pub created_ms: i64,
}

pub fn list_members(conn: &rusqlite::Connection, org_id: &str) -> SiteResult<Vec<MemberRow>> {
    let mut statement = conn.prepare(
        "SELECT u.id,u.email,u.display_name,m.role,m.created_ms FROM memberships m JOIN users u ON u.id=m.user_id WHERE m.organization_id=?1 ORDER BY m.role DESC, u.email LIMIT 200",
    )?;
    let rows = statement
        .query_map(params![org_id], |row| {
            let role_raw: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                role_raw,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(user_id, email, display_name, role, created_ms)| {
            Role::parse(&role)
                .map(|role| MemberRow {
                    user_id,
                    email,
                    display_name,
                    role,
                    created_ms,
                })
                .ok_or_else(|| SiteError::internal_message("stored role is invalid"))
        })
        .collect()
}

pub fn add_member(
    conn: &rusqlite::Connection,
    org_id: &str,
    email: &str,
    role: Role,
    now_ms: i64,
) -> SiteResult<()> {
    let user_id: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE email=?1 AND disabled=0",
            params![email],
            |row| row.get(0),
        )
        .optional()?;
    let Some(user_id) = user_id else {
        return Err(SiteError::Unprocessable(
            "No eligible account exists for that email address.",
        ));
    };
    let tx = conn.unchecked_transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT role FROM memberships WHERE user_id=?1 AND organization_id=?2",
            params![user_id, org_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.as_deref() == Some("owner") && role != Role::Owner {
        let owners: i64 = tx.query_row(
            "SELECT count(*) FROM memberships WHERE organization_id=?1 AND role='owner'",
            params![org_id],
            |row| row.get(0),
        )?;
        if owners <= 1 {
            return Err(SiteError::Conflict("The last owner cannot be demoted."));
        }
    }
    tx.execute(
        "INSERT INTO memberships (user_id,organization_id,role,created_ms) VALUES (?1,?2,?3,?4) ON CONFLICT(user_id,organization_id) DO UPDATE SET role=excluded.role",
        params![user_id, org_id, role.as_str(), now_ms],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn update_member_role(
    conn: &rusqlite::Connection,
    org_id: &str,
    user_id: &str,
    role: Role,
    now_ms: i64,
) -> SiteResult<()> {
    let tx = conn.unchecked_transaction()?;
    let changed = tx.execute(
        "UPDATE memberships SET role=?1 WHERE organization_id=?2 AND user_id=?3",
        params![role.as_str(), org_id, user_id],
    )?;
    if changed != 1 {
        return Err(SiteError::NotFound);
    }
    let owners: i64 = tx.query_row(
        "SELECT count(*) FROM memberships WHERE organization_id=?1 AND role='owner'",
        params![org_id],
        |row| row.get(0),
    )?;
    if owners == 0 {
        return Err(SiteError::Conflict(
            "Every organization must keep at least one owner.",
        ));
    }
    let _ = now_ms;
    tx.commit()?;
    Ok(())
}

pub fn remove_member(
    conn: &rusqlite::Connection,
    org_id: &str,
    user_id: &str,
    now_ms: i64,
) -> SiteResult<()> {
    let tx = conn.unchecked_transaction()?;
    let role: Option<String> = tx
        .query_row(
            "SELECT role FROM memberships WHERE organization_id=?1 AND user_id=?2",
            params![org_id, user_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(role) = role else {
        return Err(SiteError::NotFound);
    };
    if role == "owner" {
        let owners: i64 = tx.query_row(
            "SELECT count(*) FROM memberships WHERE organization_id=?1 AND role='owner'",
            params![org_id],
            |row| row.get(0),
        )?;
        if owners <= 1 {
            return Err(SiteError::Conflict("The last owner cannot be removed."));
        }
    }
    tx.execute(
        "DELETE FROM memberships WHERE organization_id=?1 AND user_id=?2",
        params![org_id, user_id],
    )?;
    tx.execute(
        "UPDATE sessions SET revoked_ms=?1 WHERE organization_id=?2 AND user_id=?3 AND revoked_ms IS NULL",
        params![now_ms, org_id, user_id],
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MembershipRow {
    pub organization_id: String,
    pub organization_name: String,
    pub role: Role,
}

pub fn memberships_for_user(
    conn: &rusqlite::Connection,
    user_id: &str,
) -> SiteResult<Vec<MembershipRow>> {
    let mut statement = conn.prepare(
        "SELECT o.id,o.name,m.role FROM memberships m JOIN organizations o ON o.id=m.organization_id WHERE m.user_id=?1 ORDER BY o.name LIMIT 50",
    )?;
    let rows = statement
        .query_map(params![user_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(organization_id, organization_name, role)| {
            Role::parse(&role)
                .map(|role| MembershipRow {
                    organization_id,
                    organization_name,
                    role,
                })
                .ok_or_else(|| SiteError::internal_message("stored role is invalid"))
        })
        .collect()
}

pub fn switch_organization(
    conn: &rusqlite::Connection,
    user_id: &str,
    session_id: &str,
    org_id: &str,
) -> SiteResult<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM memberships WHERE user_id=?1 AND organization_id=?2",
            params![user_id, org_id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(SiteError::NotFound);
    }
    conn.execute(
        "UPDATE sessions SET organization_id=?1 WHERE id=?2 AND user_id=?3",
        params![org_id, session_id, user_id],
    )?;
    Ok(())
}

pub fn rename_organization(
    conn: &rusqlite::Connection,
    org_id: &str,
    name: &str,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE organizations SET name=?1 WHERE id=?2",
        params![name, org_id],
    )?;
    Ok(())
}

pub fn invalidate_email_events(
    conn: &rusqlite::Connection,
    user_id: &str,
    kind: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "UPDATE email_events SET used_ms=?1 WHERE user_id=?2 AND kind=?3 AND used_ms IS NULL",
        params![now_ms, user_id, kind],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AdminUserRow {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub email_verified: bool,
    pub is_platform_admin: bool,
    pub disabled: bool,
    pub created_ms: i64,
    pub organization_count: i64,
}

pub fn admin_users(conn: &rusqlite::Connection) -> SiteResult<Vec<AdminUserRow>> {
    let mut statement = conn.prepare(
        "SELECT u.id,u.email,u.display_name,u.email_verified,u.is_platform_admin,u.disabled,u.created_ms,(SELECT count(*) FROM memberships m WHERE m.user_id=u.id) FROM users u ORDER BY u.created_ms DESC LIMIT 500",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(AdminUserRow {
                id: row.get(0)?,
                email: row.get(1)?,
                display_name: row.get(2)?,
                email_verified: row.get::<_, i64>(3)? != 0,
                is_platform_admin: row.get::<_, i64>(4)? != 0,
                disabled: row.get::<_, i64>(5)? != 0,
                created_ms: row.get(6)?,
                organization_count: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct AdminOrgRow {
    pub id: String,
    pub name: String,
    pub created_ms: i64,
    pub member_count: i64,
}

pub fn admin_orgs(conn: &rusqlite::Connection) -> SiteResult<Vec<AdminOrgRow>> {
    let mut statement = conn.prepare(
        "SELECT o.id,o.name,o.created_ms,(SELECT count(*) FROM memberships m WHERE m.organization_id=o.id) FROM organizations o ORDER BY o.created_ms DESC LIMIT 500",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(AdminOrgRow {
                id: row.get(0)?,
                name: row.get(1)?,
                created_ms: row.get(2)?,
                member_count: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn set_user_disabled(
    conn: &rusqlite::Connection,
    user_id: &str,
    disabled: bool,
    now_ms: i64,
) -> SiteResult<()> {
    let changed = conn.execute(
        "UPDATE users SET disabled=?1,updated_ms=?2 WHERE id=?3",
        params![i64::from(disabled), now_ms, user_id],
    )?;
    if changed != 1 {
        return Err(SiteError::NotFound);
    }
    if disabled {
        revoke_user_sessions(conn, user_id, now_ms, None)?;
    }
    Ok(())
}

pub fn audit(
    conn: &rusqlite::Connection,
    actor_user_id: &str,
    organization_id: Option<&str>,
    action: &str,
    target: &str,
    detail: &str,
    now_ms: i64,
) -> SiteResult<()> {
    conn.execute(
        "INSERT INTO admin_audit (id,actor_user_id,organization_id,action,target,detail,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            format!("aud_{}", Uuid::new_v4()),
            actor_user_id,
            organization_id,
            action,
            target,
            detail,
            now_ms
        ],
    )?;
    Ok(())
}
