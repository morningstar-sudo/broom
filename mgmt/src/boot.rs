// boot.rs — M5 boot-menu. Handler GET /boot.ipxe: sinh iPXE menu động.
// iPXE gọi: chain http://SERVER/boot.ipxe?mac=${net0/mac}
// Menu liệt kê image, gắn item default + countdown; hết giờ → boot default.
use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use std::collections::HashMap;

use crate::{db, SharedState};

pub async fn render(
    State(st): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let conn = st.db.lock().unwrap();
    let _mac = q.get("mac").cloned().unwrap_or_default(); // TODO: lọc image theo máy

    let timeout_s: u64 = db::get_config(&conn, "boot_timeout", "10")
        .parse()
        .unwrap_or(10);
    let timeout_ms = timeout_s * 1000;

    // (name, os, is_default, boot_script)
    let mut stmt = conn
        .prepare("SELECT name, os, is_default, boot_script FROM images ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap();

    let mut menu_items = String::new();
    let mut targets = String::new();
    let mut default_label = String::new();
    for row in rows {
        let (name, os, is_def, boot_script) = row.unwrap();
        let label = sanitize(&name);
        // iPXE console chỉ ASCII → tránh dấu tiếng Việt / em-dash.
        menu_items.push_str(&format!("item {label} [{os}] {name}\n"));
        let body = match boot_script.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            // Chưa cấu hình boot_script → báo + về menu (không treo).
            _ => format!(
                "echo Image '{name}' chua co boot_script — dat qua /api/images/boot-script\nsleep 3\ngoto start"
            ),
        };
        targets.push_str(&format!(":{label}\n{body}\n\n"));
        if is_def == 1 {
            default_label = label;
        }
    }

    if menu_items.is_empty() {
        menu_items.push_str("item shell iPXE shell (chua co image)\n");
        default_label = "shell".into();
    }
    if default_label.is_empty() {
        // Chưa image nào đặt default → rơi về shell khi hết countdown (không auto-boot nhầm OS).
        default_label = "shell".into();
    }

    let script = format!(
        "#!ipxe\n\
         :start\n\
         menu Bootrom Tiem Net - chon he dieu hanh\n\
         {menu_items}\
         choose --default {default_label} --timeout {timeout_ms} sel && goto ${{sel}}\n\n\
         {targets}\
         :shell\nshell\n"
    );

    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], script)
}

/// iPXE label chỉ nên [A-Za-z0-9_]. Đổi ký tự khác thành '_'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
