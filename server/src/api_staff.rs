use crate::api_public::{menu_list, order_view};
use crate::auth::Auth;
use crate::orders;
use crate::util::*;
use crate::{sse, App};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response, Sse},
    Json,
};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

type S = Arc<App>;

// ---------- stream notifikasi staf ----------

pub async fn staff_events(
    State(s): State<S>,
    auth: Auth,
    headers: HeaderMap,
) -> R<Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>> {
    auth.need(&["kasir", "dapur"])?;
    let auds: Vec<String> = match auth.role.as_str() {
        "dapur" => vec!["dapur".into()],
        "kasir" => vec!["kasir".into()],
        _ => vec!["dapur".into(), "kasir".into()],
    };
    let last = {
        let conn = s.db.lock().unwrap();
        sse::start_id(&conn, &headers)
    };
    Ok(sse::stream(s.clone(), None, auds, last))
}

// ---------- layar dapur ----------

pub async fn dapur(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&["dapur"])?;
    let conn = s.db.lock().unwrap();
    let telat = setting_i(&conn, "telat_menit", 15);
    let n = now();
    let d0 = day_start(n, s.cfg.tz);

    let mut stmt = conn.prepare(
        "SELECT id, meja_no, status, paid_at, ready_at FROM orders
          WHERE status IN ('baru','dimasak','siap') ORDER BY paid_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let (mut baru, mut masak, mut siap) = (Vec::new(), Vec::new(), Vec::new());
    for (id, meja, status, paid_at, ready_at) in rows {
        let base = (if status == "siap" { ready_at } else { paid_at }).unwrap_or(n);
        let mnt = ((n - base) / 60).max(0);
        let o = json!({
            "id": id, "kode": kode(id), "meja": meja, "status": status, "mnt": mnt,
            "paid_at": paid_at, "telat": status != "siap" && mnt >= telat,
            "items": orders::items_json(&conn, id)?,
        });
        match status.as_str() {
            "baru" => baru.push(o),
            "dimasak" => masak.push(o),
            _ => siap.push(o),
        }
    }
    let rata: Option<f64> = conn.query_row(
        "SELECT AVG(ready_at - cooking_at) FROM orders WHERE ready_at IS NOT NULL AND cooking_at IS NOT NULL AND created_at >= ?1",
        params![d0], |r| r.get(0),
    )?;
    let selesai: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE ready_at >= ?1", params![d0], |r| r.get(0),
    )?;
    Ok(Json(json!({
        "baru": baru, "masak": masak, "siap": siap,
        "stat": { "rata_masak": rata.map(|v| (v / 60.0).round() as i64).unwrap_or(0), "selesai_hari_ini": selesai }
    })))
}

#[derive(Deserialize)]
pub struct StatusIn {
    to: String,
}

pub async fn set_status(State(s): State<S>, auth: Auth, Path(id): Path<i64>, Json(b): Json<StatusIn>) -> R<Json<Value>> {
    // dapur: baru->dimasak->siap. Kasir/admin boleh menandai "selesai" (sudah diantar).
    match b.to.as_str() {
        "dimasak" | "siap" => auth.need(&["dapur"])?,
        "selesai" => auth.need(&["kasir", "dapur"])?,
        _ => return Err(bad("status tujuan tidak dikenal")),
    }
    let conn = s.db.lock().unwrap();
    orders::advance_status(&conn, id, &b.to)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- layar kasir ----------

fn list_orders(conn: &Connection, filter: &str, limit: i64) -> R<Vec<Value>> {
    let where_ = match filter {
        "belum" => "WHERE status = 'menunggu_bayar'",
        "lunas" => "WHERE paid_at IS NOT NULL AND status != 'batal'",
        _ => "",
    };
    let sql = format!(
        "SELECT id, meja_no, metode, status, total, paid_at, created_at FROM orders {where_} ORDER BY id DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((
                r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, i64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (id, meja, metode, status, total, paid_at, created_at) in rows {
        out.push(json!({
            "id": id, "kode": kode(id), "meja": meja, "metode": metode, "status": status,
            "total": total, "paid_at": paid_at, "created_at": created_at,
            "isi": orders::items_text(conn, id)?,
        }));
    }
    Ok(out)
}

pub async fn kasir(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    let n = now();
    let d0 = day_start(n, s.cfg.tz);
    // Fix B + C: omzet bersih = pembayaran dikurangi pengembalian dana, memakai
    // rumus yang sama persis dengan layar admin supaya kedua angka tidak berbeda.
    let sum = |m: &str| -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT (SELECT COALESCE(SUM(jumlah),0) FROM payments WHERE metode = ?1 AND created_at >= ?2)
                  - (SELECT COALESCE(SUM(jumlah),0) FROM refunds  WHERE metode = ?1 AND created_at >= ?2)",
            params![m, d0], |r| r.get(0),
        )
    };
    let qris = sum("qris")?;
    let tunai = sum("tunai")?;
    let refund: i64 = conn.query_row(
        "SELECT COALESCE(SUM(jumlah),0) FROM refunds WHERE created_at >= ?1",
        params![d0], |r| r.get(0),
    )?;
    let aktif: i64 = conn.query_row("SELECT COUNT(*) FROM orders WHERE status IN ('baru','dimasak','siap')", [], |r| r.get(0))?;
    let menunggu: i64 = conn.query_row("SELECT COUNT(*) FROM orders WHERE status = 'menunggu_bayar'", [], |r| r.get(0))?;

    // denah meja: bayar > masak > makan > kosong
    let mut stmt = conn.prepare("SELECT no FROM meja WHERE aktif = 1 ORDER BY no")?;
    let nos: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    let mut meja = Vec::new();
    for no in nos {
        let mut st = conn.prepare(
            "SELECT status, metode, paid_at, served_at FROM orders
              WHERE meja_no = ?1 AND (status IN ('menunggu_bayar','baru','dimasak','siap')
                    OR (status = 'selesai' AND dibersihkan = 0))",
        )?;
        let rows = st
            .query_map(params![no], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let (mut state, mut info, mut prio) = ("kosong", String::new(), 0);
        for (status, metode, paid_at, served_at) in rows {
            let (p, s_, i) = match status.as_str() {
                "menunggu_bayar" => (4, "bayar", metode.clone()),
                "selesai" => (2, "makan", format!("{} mnt", ((n - served_at.unwrap_or(n)) / 60).max(0))),
                _ => (3, "masak", format!("{} mnt", ((n - paid_at.unwrap_or(n)) / 60).max(0))),
            };
            if p > prio {
                prio = p;
                state = s_;
                info = i;
            }
        }
        meja.push(json!({ "no": no, "state": state, "info": info }));
    }
    Ok(Json(json!({
        "stat": { "qris": qris, "tunai": tunai, "aktif": aktif, "menunggu": menunggu, "refund": refund },
        "meja": meja,
        "nama": auth.nama,
    })))
}

#[derive(Deserialize)]
pub struct ListQ {
    filter: Option<String>,
    limit: Option<i64>,
}

pub async fn orders_list(State(s): State<S>, auth: Auth, Query(q): Query<ListQ>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    let list = list_orders(&conn, q.filter.as_deref().unwrap_or(""), q.limit.unwrap_or(30).clamp(1, 200))?;
    Ok(Json(json!(list)))
}

pub async fn order_detail(State(s): State<S>, auth: Auth, Path(id): Path<i64>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    Ok(Json(order_view(&conn, id)?))
}

#[derive(Deserialize)]
pub struct Tunai {
    uang: i64,
}

pub async fn bayar_tunai(State(s): State<S>, auth: Auth, Path(id): Path<i64>, Json(b): Json<Tunai>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    let total: i64 = conn
        .query_row("SELECT total FROM orders WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|_| not_found("pesanan tidak ditemukan"))?;
    if b.uang < total {
        return Err(bad("uang yang diterima kurang dari total"));
    }
    let baru = orders::mark_paid(&conn, id, "tunai", b.uang, &format!("CASH-{id}"), Some(b.uang), Some(b.uang - total), Some(auth.nama.as_str()))?;
    if !baru {
        // Sudah lunas sebelumnya. Jangan balas sukses: kasir bisa menyerahkan kembalian dua kali.
        return Err(conflict("pesanan ini sudah lunas"));
    }
    Ok(Json(json!({ "ok": true, "kembalian": b.uang - total })))
}

/// Konfirmasi manual kalau webhook QRIS terlambat tapi kasir sudah melihat dana masuk.
pub async fn konfirmasi_qris(State(s): State<S>, auth: Auth, Path(id): Path<i64>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    let total: i64 = conn
        .query_row("SELECT total FROM orders WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|_| not_found("pesanan tidak ditemukan"))?;
    let baru = orders::mark_paid(&conn, id, "qris", total, &format!("MANUAL-{id}"), None, None, Some(auth.nama.as_str()))?;
    if !baru {
        return Err(conflict("pesanan ini sudah lunas"));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn batal(State(s): State<S>, auth: Auth, Path(id): Path<i64>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    orders::cancel(&conn, id, true, Some(auth.nama.as_str()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn kosongkan(State(s): State<S>, auth: Auth, Path(no): Path<String>) -> R<Json<Value>> {
    auth.need(&["kasir"])?;
    let conn = s.db.lock().unwrap();
    conn.execute("UPDATE orders SET dibersihkan = 1 WHERE meja_no = ?1 AND status = 'selesai'", params![no])?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- admin ----------

fn ymd(t: i64, tz_min: i64) -> String {
    // hari sejak 1970-01-01 -> tanggal sipil (algoritma Howard Hinnant)
    let z = (t + tz_min * 60).div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Omzet bersih satu periode: pembayaran dikurangi pengembalian dana yang terjadi
/// di periode itu. Tidak memakai status pesanan, supaya pembatalan hari ini tidak
/// mengubah angka hari kemarin yang sudah terlanjur dilaporkan.
fn omzet_between(conn: &Connection, a: i64, b: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT (SELECT COALESCE(SUM(jumlah),0) FROM payments WHERE created_at >= ?1 AND created_at < ?2)
              - (SELECT COALESCE(SUM(jumlah),0) FROM refunds  WHERE created_at >= ?1 AND created_at < ?2)",
        params![a, b], |r| r.get(0),
    )
}

pub async fn ringkasan(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let n = now();
    let d0 = day_start(n, s.cfg.tz);
    let omzet = omzet_between(&conn, d0, d0 + 86400)?;
    let kemarin = omzet_between(&conn, d0 - 86400, d0)?;
    let naik_pct: Option<i64> = if kemarin > 0 {
        Some(((omzet - kemarin) as f64 * 100.0 / kemarin as f64).round() as i64)
    } else {
        None
    };
    let pesanan: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE paid_at >= ?1 AND status != 'batal'", params![d0], |r| r.get(0))?;
    let aktif: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE status IN ('baru','dimasak','siap')", [], |r| r.get(0))?;
    let rata: Option<f64> = conn.query_row(
        "SELECT AVG(ready_at - cooking_at) FROM orders WHERE ready_at IS NOT NULL AND cooking_at IS NOT NULL AND created_at >= ?1",
        params![d0], |r| r.get(0))?;
    let refund: i64 = conn.query_row(
        "SELECT COALESCE(SUM(jumlah),0) FROM refunds WHERE created_at >= ?1", params![d0], |r| r.get(0))?;
    let qris_n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE paid_at >= ?1 AND metode = 'qris' AND status != 'batal'", params![d0], |r| r.get(0))?;
    let tunai_n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE paid_at >= ?1 AND metode = 'tunai' AND status != 'batal'", params![d0], |r| r.get(0))?;

    // pesanan per jam 10..21 (waktu lokal restoran)
    let mut per_jam = vec![0i64; 12];
    let mut stmt = conn.prepare("SELECT paid_at FROM orders WHERE paid_at >= ?1 AND status != 'batal'")?;
    let times: Vec<i64> = stmt.query_map(params![d0], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    for t in times {
        let jam = ((t + s.cfg.tz * 60).div_euclid(3600)).rem_euclid(24);
        if (10..=21).contains(&jam) {
            per_jam[(jam - 10) as usize] += 1;
        }
    }
    let mut stmt = conn.prepare(
        "SELECT oi.nama, SUM(oi.qty) AS q FROM order_items oi JOIN orders o ON o.id = oi.order_id
          WHERE o.paid_at >= ?1 AND o.status != 'batal' GROUP BY oi.menu_id ORDER BY q DESC LIMIT 5",
    )?;
    let laris = stmt
        .query_map(params![d0], |r| Ok(json!({ "nama": r.get::<_, String>(0)?, "jml": r.get::<_, i64>(1)? })))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "omzet": omzet, "naik_pct": naik_pct, "pesanan": pesanan, "aktif": aktif, "refund": refund,
        "rata_masak": rata.map(|v| (v / 60.0).round() as i64).unwrap_or(0),
        "target_masak": setting_i(&conn, "target_masak", 15),
        "qris_n": qris_n, "tunai_n": tunai_n, "per_jam": per_jam, "laris": laris,
    })))
}

pub async fn admin_orders(State(s): State<S>, auth: Auth, Query(q): Query<ListQ>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    Ok(Json(json!(list_orders(&conn, q.filter.as_deref().unwrap_or(""), q.limit.unwrap_or(100).clamp(1, 500))?)))
}

#[derive(Deserialize)]
pub struct ExtraIn {
    nama: String,
    harga: i64,
}

#[derive(Deserialize)]
pub struct MenuIn {
    nama: String,
    kategori: String,
    harga: i64,
    #[serde(default)]
    deskripsi: String,
    #[serde(default)]
    foto_url: String,
    #[serde(default)]
    pedas: bool,
    waktu_masak: Option<i64>,
    #[serde(default)]
    extras: Vec<ExtraIn>,
}

fn check_menu(b: &MenuIn) -> R<()> {
    if b.nama.trim().is_empty() || b.nama.len() > 80 {
        return Err(bad("nama menu wajib diisi (maks 80 karakter)"));
    }
    if b.kategori.trim().is_empty() || b.kategori.len() > 30 {
        return Err(bad("kategori wajib diisi"));
    }
    if b.harga < 0 || b.harga > 10_000_000 {
        return Err(bad("harga tidak valid"));
    }
    let f = b.foto_url.trim();
    if !(f.is_empty() || f.starts_with("https://") || f.starts_with("http://") || f.starts_with('/')) {
        return Err(bad("URL foto harus diawali http(s):// atau /"));
    }
    if b.extras.len() > 10 || b.extras.iter().any(|e| e.nama.trim().is_empty() || e.harga < 0) {
        return Err(bad("pilihan tambahan tidak valid"));
    }
    Ok(())
}

fn save_extras(conn: &Connection, menu_id: i64, extras: &[ExtraIn]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM menu_extras WHERE menu_id = ?1", params![menu_id])?;
    for e in extras {
        conn.execute(
            "INSERT INTO menu_extras(menu_id, nama, harga) VALUES (?1, ?2, ?3)",
            params![menu_id, e.nama.trim(), e.harga],
        )?;
    }
    Ok(())
}

pub async fn admin_menu(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    Ok(Json(json!(menu_list(&conn)?)))
}

pub async fn menu_create(State(s): State<S>, auth: Auth, Json(b): Json<MenuIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    check_menu(&b)?;
    let conn = s.db.lock().unwrap();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO menu_items(nama, kategori, harga, deskripsi, foto_url, pedas, waktu_masak, urutan)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, (SELECT COALESCE(MAX(urutan),0)+1 FROM menu_items))",
        params![b.nama.trim(), b.kategori.trim(), b.harga, b.deskripsi.trim(), b.foto_url.trim(),
                b.pedas as i64, b.waktu_masak.unwrap_or(10).clamp(1, 120)],
    )?;
    let id = tx.last_insert_rowid();
    save_extras(&tx, id, &b.extras)?;
    tx.commit()?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

pub async fn menu_update(State(s): State<S>, auth: Auth, Path(id): Path<i64>, Json(b): Json<MenuIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    check_menu(&b)?;
    let conn = s.db.lock().unwrap();
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE menu_items SET nama=?2, kategori=?3, harga=?4, deskripsi=?5, foto_url=?6, pedas=?7, waktu_masak=?8
          WHERE id=?1 AND aktif=1",
        params![id, b.nama.trim(), b.kategori.trim(), b.harga, b.deskripsi.trim(), b.foto_url.trim(),
                b.pedas as i64, b.waktu_masak.unwrap_or(10).clamp(1, 120)],
    )?;
    if n == 0 {
        return Err(not_found("menu tidak ditemukan"));
    }
    save_extras(&tx, id, &b.extras)?;
    tx.commit()?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TersediaIn {
    tersedia: bool,
}

pub async fn menu_tersedia(State(s): State<S>, auth: Auth, Path(id): Path<i64>, Json(b): Json<TersediaIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    conn.execute("UPDATE menu_items SET tersedia = ?2 WHERE id = ?1", params![id, b.tersedia as i64])?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn menu_hapus(State(s): State<S>, auth: Auth, Path(id): Path<i64>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    conn.execute("UPDATE menu_items SET aktif = 0 WHERE id = ?1", params![id])?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct MejaIn {
    no: String,
}

pub async fn admin_meja(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let mut stmt = conn.prepare("SELECT no, aktif FROM meja ORDER BY no")?;
    let rows = stmt
        .query_map([], |r| Ok(json!({ "no": r.get::<_, String>(0)?, "aktif": r.get::<_, i64>(1)? == 1 })))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!(rows)))
}

pub async fn meja_tambah(State(s): State<S>, auth: Auth, Json(b): Json<MejaIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    let no = b.no.trim().to_string();
    if no.is_empty() || no.len() > 8 || !no.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(bad("nomor meja: huruf/angka/tanda minus, maks 8 karakter"));
    }
    let conn = s.db.lock().unwrap();
    conn.execute("INSERT OR IGNORE INTO meja(no) VALUES (?1)", params![no])?;
    conn.execute("UPDATE meja SET aktif = 1 WHERE no = ?1", params![no])?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn meja_nonaktif(State(s): State<S>, auth: Auth, Path(no): Path<String>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    conn.execute("UPDATE meja SET aktif = 0 WHERE no = ?1", params![no])?;
    Ok(Json(json!({ "ok": true })))
}

fn laporan_rows(conn: &Connection, tz: i64, hari: i64) -> rusqlite::Result<Vec<Value>> {
    let d0 = day_start(now(), tz);
    let mut out = Vec::new();
    for i in 0..hari {
        let a = d0 - i * 86400;
        let b = a + 86400;
        let per = |m: &str| -> rusqlite::Result<i64> {
            conn.query_row(
                "SELECT (SELECT COALESCE(SUM(jumlah),0) FROM payments WHERE metode = ?1 AND created_at >= ?2 AND created_at < ?3)
                      - (SELECT COALESCE(SUM(jumlah),0) FROM refunds  WHERE metode = ?1 AND created_at >= ?2 AND created_at < ?3)",
                params![m, a, b], |r| r.get(0),
            )
        };
        let qris = per("qris")?;
        let tunai = per("tunai")?;
        let refund: i64 = conn.query_row(
            "SELECT COALESCE(SUM(jumlah),0) FROM refunds WHERE created_at >= ?1 AND created_at < ?2",
            params![a, b], |r| r.get(0),
        )?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM orders WHERE paid_at >= ?1 AND paid_at < ?2 AND status != 'batal'",
            params![a, b], |r| r.get(0),
        )?;
        out.push(json!({ "tanggal": ymd(a, tz), "pesanan": n, "qris": qris, "tunai": tunai,
                         "refund": refund, "total": qris + tunai }));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct HariQ {
    hari: Option<i64>,
}

pub async fn laporan(State(s): State<S>, auth: Auth, Query(q): Query<HariQ>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    Ok(Json(json!(laporan_rows(&conn, s.cfg.tz, q.hari.unwrap_or(7).clamp(1, 90))?)))
}

pub async fn laporan_csv(State(s): State<S>, auth: Auth, Query(q): Query<HariQ>) -> R<Response> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let rows = laporan_rows(&conn, s.cfg.tz, q.hari.unwrap_or(30).clamp(1, 365))?;
    let mut csv = String::from("tanggal,pesanan,omzet_qris,omzet_tunai,pengembalian,omzet_total\n");
    for r in rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{}\n",
            r["tanggal"].as_str().unwrap_or(""), r["pesanan"], r["qris"], r["tunai"], r["refund"], r["total"]
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"laporan-dapurku.csv\"".to_string()),
        ],
        csv,
    )
        .into_response())
}

// ---------- pengguna staf (admin) ----------

#[derive(Deserialize)]
pub struct UserIn {
    username: Option<String>,
    password: Option<String>,
    role: Option<String>,
    nama: Option<String>,
    aktif: Option<bool>,
}

fn cek_username(u: &str) -> R<String> {
    let u = u.trim().to_lowercase();
    let bentuk_ok = u
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_');
    if u.len() < 3 || u.len() > 32 || !bentuk_ok {
        return Err(bad("username 3-32 karakter: huruf kecil, angka, titik, strip, garis bawah"));
    }
    Ok(u)
}

fn cek_password(p: &str) -> R<()> {
    if p.chars().count() < 8 {
        return Err(bad("password minimal 8 karakter"));
    }
    if p.len() > 200 {
        return Err(bad("password terlalu panjang"));
    }
    Ok(())
}

fn cek_role(r: &str) -> R<()> {
    if !["kasir", "dapur", "admin"].contains(&r) {
        return Err(bad("role harus kasir, dapur, atau admin"));
    }
    Ok(())
}

fn cek_nama(n: &str) -> R<String> {
    let n: String = n.trim().chars().take(40).collect();
    if n.is_empty() {
        return Err(bad("nama wajib diisi"));
    }
    Ok(n)
}

/// Jumlah admin aktif selain `kecuali`. Dipakai supaya admin terakhir tidak bisa
/// menonaktifkan atau menurunkan dirinya sendiri sampai tidak ada yang bisa masuk.
fn admin_aktif_lain(conn: &Connection, kecuali: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND aktif = 1 AND id != ?1",
        params![kecuali],
        |r| r.get(0),
    )
}

pub async fn users_list(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, username, role, nama, aktif, created_at, last_login FROM users ORDER BY role, username",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "username": r.get::<_, String>(1)?,
                "role": r.get::<_, String>(2)?, "nama": r.get::<_, String>(3)?,
                "aktif": r.get::<_, i64>(4)? == 1, "created_at": r.get::<_, i64>(5)?,
                "last_login": r.get::<_, Option<i64>>(6)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!(rows)))
}

pub async fn user_create(State(s): State<S>, auth: Auth, Json(b): Json<UserIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    let username = cek_username(&b.username.unwrap_or_default())?;
    let password = b.password.unwrap_or_default();
    cek_password(&password)?;
    let role = b.role.unwrap_or_default();
    cek_role(&role)?;
    let nama = cek_nama(&b.nama.unwrap_or_default())?;
    let conn = s.db.lock().unwrap();
    let ada: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE username = ?1",
        params![username],
        |r| r.get(0),
    )?;
    if ada > 0 {
        return Err(conflict("username sudah dipakai"));
    }
    conn.execute(
        "INSERT INTO users(username, pass_hash, role, nama, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![username, crate::auth::hash_password(&password), role, nama, now()],
    )?;
    Ok(Json(json!({ "ok": true, "id": conn.last_insert_rowid() })))
}

pub async fn user_update(State(s): State<S>, auth: Auth, Path(id): Path<i64>, Json(b): Json<UserIn>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let (role_kini, aktif_kini): (String, i64) = conn
        .query_row("SELECT role, aktif FROM users WHERE id = ?1", params![id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| not_found("pengguna tidak ditemukan"))?;

    let role_baru = match &b.role {
        Some(r) => {
            cek_role(r)?;
            r.clone()
        }
        None => role_kini.clone(),
    };
    let aktif_baru = b.aktif.unwrap_or(aktif_kini == 1);
    let masih_admin_aktif = role_baru == "admin" && aktif_baru;
    if role_kini == "admin" && aktif_kini == 1 && !masih_admin_aktif && admin_aktif_lain(&conn, id)? == 0 {
        return Err(conflict("harus ada minimal satu admin yang aktif"));
    }

    if let Some(n) = &b.nama {
        let n = cek_nama(n)?;
        conn.execute("UPDATE users SET nama = ?2 WHERE id = ?1", params![id, n])?;
    }
    conn.execute(
        "UPDATE users SET role = ?2, aktif = ?3 WHERE id = ?1",
        params![id, role_baru, aktif_baru as i64],
    )?;
    if let Some(p) = &b.password {
        cek_password(p)?;
        conn.execute(
            "UPDATE users SET pass_hash = ?2, pass_bawaan = 0 WHERE id = ?1",
            params![id, crate::auth::hash_password(p)],
        )?;
    }
    // Ganti password, ganti role, atau dinonaktifkan: sesi lama tidak boleh dipakai
    // lagi. Sesi yang sedang dipakai untuk permintaan ini dikecualikan, supaya admin
    // yang mengganti password dirinya sendiri tidak langsung terlempar keluar.
    if b.password.is_some() || role_baru != role_kini || !aktif_baru {
        conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token != ?2",
            params![id, auth.token],
        )?;
    }
    Ok(Json(json!({ "ok": true })))
}

const SETTING_KEYS: [(&str, bool); 6] = [
    ("nama_resto", false),
    ("merchant_nmid", false),
    ("bayar_menit", true),
    ("tunai_menit", true),
    ("target_masak", true),
    ("telat_menit", true),
];

pub async fn settings_get(State(s): State<S>, auth: Auth) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    let mut m = serde_json::Map::new();
    for (k, _) in SETTING_KEYS {
        m.insert(k.to_string(), json!(setting_s(&conn, k, "")));
    }
    Ok(Json(Value::Object(m)))
}

pub async fn settings_put(State(s): State<S>, auth: Auth, Json(b): Json<HashMap<String, String>>) -> R<Json<Value>> {
    auth.need(&[])?;
    let conn = s.db.lock().unwrap();
    for (k, numeric) in SETTING_KEYS {
        if let Some(v) = b.get(k) {
            let v = v.trim();
            if numeric {
                match v.parse::<i64>() {
                    Ok(n) if (1..=240).contains(&n) => {}
                    _ => return Err(bad(&format!("{k} harus angka 1-240"))),
                }
            } else if v.is_empty() || v.len() > 60 {
                return Err(bad(&format!("{k} tidak boleh kosong (maks 60 karakter)")));
            }
            conn.execute(
                "INSERT INTO settings(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}
