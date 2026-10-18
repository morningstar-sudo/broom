// users.rs — the accounts (Settings page, owners only: auth.rs staff_may keeps staff out): list, add, change role /
// reset password, delete. There is always an owner left.
use axum::{
    Extension, Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::Deserialize;

use crate::SharedState;
use crate::api::{ApiError, bad, ok};
use crate::auth::{Role, User, Who, name_ok, new_user, users, with_password};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/users", get(list).post(set))
        .route("/api/users/delete", post(delete))
}

/// GET /api/users — [{name, role}].
async fn list(State(st): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!(
        users(&st)
            .iter()
            .map(|u| serde_json::json!({"name": u.name, "role": u.role}))
            .collect::<Vec<_>>()
    ))
}

#[derive(Deserialize)]
struct SetBody {
    name: String,
    role: Role,
    /// The password's verifier, sealed with the caller's session key (pwproof.rs; the password never crosses the
    /// network). New account: required. Existing one: a new password (its sessions end); absent = unchanged.
    #[serde(default)]
    cred: Option<crate::pwproof::Sealed>,
}

/// No owner left after this change.
fn no_owner(v: &[User]) -> bool {
    !v.iter().any(|u| u.role == Role::Owner)
}

/// POST /api/users {name, role, cred?} — add an account, or change one's role / password.
async fn set(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    headers: axum::http::HeaderMap,
    Json(b): Json<SetBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_lowercase();
    if !name_ok(&name) {
        return Err(bad("user name: a-z, 0-9, _ or -, at most 32"));
    }
    let new_pw = b.cred.is_some();
    let phc = match &b.cred {
        None => None,
        Some(c) => Some(
            crate::auth::session_key(&st, &headers)
                .and_then(|k| crate::pwproof::open(&k, &name, c))
                .ok_or_else(|| bad("password: bad seal — sign in again"))?,
        ),
    };
    // As one step: the "an owner is left" rule holds even when two owners edit at once.
    let added = crate::auth::edit_users(&st, |all| {
        let added = match all.iter().position(|u| u.name == name) {
            Some(i) => {
                let mut u = all[i].clone();
                u.role = b.role;
                if let Some(p) = phc {
                    u = with_password(u, p);
                }
                all[i] = u;
                false
            }
            None => {
                let Some(p) = phc else {
                    return Err(bad("a new account needs a password"));
                };
                all.push(new_user(&name, b.role, p));
                true
            }
        };
        if no_owner(all) {
            return Err(bad("there must be an owner left"));
        }
        Ok(added)
    })?;
    if !added && new_pw {
        crate::auth::end_sessions(&st, &name); // a new password ends its sessions
    }
    if added {
        tracing::warn!("{}: account {name} added ({:?})", w.name, b.role);
    } else {
        tracing::warn!(
            "{}: account {name} set to {:?}{}",
            w.name,
            b.role,
            if !new_pw { "" } else { ", new password" }
        );
    }
    Ok(ok())
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

/// POST /api/users/delete {name} — not your own account, not the last owner. Its sessions end.
async fn delete(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if b.name == w.name {
        return Err(bad("you can't delete your own account"));
    }
    crate::auth::edit_users(&st, |all| {
        all.retain(|u| u.name != b.name);
        if no_owner(all) {
            return Err(bad("there must be an owner left"));
        }
        Ok(())
    })?;
    crate::auth::end_sessions(&st, &b.name);
    tracing::warn!("{}: account {} deleted", w.name, b.name);
    Ok(ok())
}
