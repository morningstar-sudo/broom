// users.rs — the accounts (Settings page, owners only: auth.rs staff_may keeps staff out): list, add, change role /
// reset password, delete. There is always an owner left.
use axum::{
    Extension, Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::Deserialize;

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};
use crate::auth::{Role, User, Who, name_ok, new_user, save_users, users, with_password};

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
    /// New account: required. Existing one: a new password (its sessions end); empty = unchanged.
    #[serde(default)]
    password: String,
}

/// No owner left after this change.
fn no_owner(v: &[User]) -> bool {
    !v.iter().any(|u| u.role == Role::Owner)
}

/// POST /api/users {name, role, password} — add an account, or change one's role / password.
async fn set(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    Json(b): Json<SetBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_lowercase();
    if !name_ok(&name) {
        return Err(bad("user name: a-z, 0-9, _ or -, at most 32"));
    }
    if !b.password.is_empty() && b.password.len() < 8 {
        return Err(bad("password: at least 8 characters"));
    }
    let phc = if b.password.is_empty() {
        None
    } else {
        let pw = b.password.clone();
        Some(
            tokio::task::spawn_blocking(move || crate::auth::hash_password(&pw))
                .await
                .map_err(ise)?
                .map_err(ise)?,
        )
    };
    let mut all = users(&st);
    match all.iter().position(|u| u.name == name) {
        Some(i) => {
            let mut u = all[i].clone();
            u.role = b.role;
            if let Some(p) = phc {
                u = with_password(u, p);
            }
            all[i] = u;
            if no_owner(&all) {
                return Err(bad("there must be an owner left"));
            }
            tracing::warn!(
                "{}: account {name} set to {:?}{}",
                w.name,
                b.role,
                if b.password.is_empty() {
                    ""
                } else {
                    ", new password"
                }
            );
        }
        None => {
            let Some(p) = phc else {
                return Err(bad("a new account needs a password"));
            };
            all.push(new_user(&name, b.role, p));
            tracing::warn!("{}: account {name} added ({:?})", w.name, b.role);
        }
    }
    save_users(&st, &all).map_err(ise)?;
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
    let all: Vec<User> = users(&st)
        .into_iter()
        .filter(|u| u.name != b.name)
        .collect();
    if no_owner(&all) {
        return Err(bad("there must be an owner left"));
    }
    save_users(&st, &all).map_err(ise)?;
    tracing::warn!("{}: account {} deleted", w.name, b.name);
    Ok(ok())
}
