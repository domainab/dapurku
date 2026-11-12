use crate::auth::{cookie, Auth};
use crate::notify::push;
use crate::orders::{self, NewOrder};
use crate::util::*;
use crate::{sse, App};
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response, Sse},
    Json,
};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::Arc;

type S = Arc<App>;

// ---------- konfigurasi & menu ----------

pub async fn config(State(s): State<S>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    Ok(Json(json!({
        "nama_resto": setting_s(&conn, "nama_resto", "Dapurku"),
        "merchant_nmid": setting_s(&conn, "merchant_nmid", "SIMULASI"),
        "bayar_menit": setting_i(&conn, "bayar_menit", 15),
        "sim": s.cfg.sim,
    })))
}

fn extras_of(conn: &Connection, menu_id: i64) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT id, nama, harga FROM menu_extras WHERE menu_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![menu_id], |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "nama": r.get::<_, String>(1)?, "harga": r.get::<_, i64>(2)? }))
    })?;
    rows.collect()
}

pub fn menu_list(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, nama, kategori, harga, deskripsi, foto_url, pedas, waktu_masak, tersedia
           FROM menu_items WHERE aktif = 1 ORDER BY urutan, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, i64>(6)?, r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    let list: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (id, nama, kat, harga, desk, foto, pedas, wm, ada) in list {
        out.push(json!({
            "id": id, "nama": nama, "kategori": kat, "harga": harga, "deskripsi": desk,
            "foto_url": foto, "pedas": pedas == 1, "waktu_masak": wm, "tersedia": ada == 1,
            "extras": extras_of(conn, id)?,
        }));
    }
    Ok(out)
}

pub async fn menu(State(s): State<S>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let items = menu_list(&conn)?;
    let d0 = day_start(now(), s.cfg.tz);
    // Terlaris hari ini; kalau belum ada penjualan, pakai menu pertama yang tersedia.
    let top: Option<i64> = conn
        .query_row(
            "SELECT oi.menu_id FROM order_items oi JOIN orders o ON o.id = oi.order_id
              WHERE o.paid_at >= ?1 AND o.status NOT IN ('batal','kedaluwarsa')
              GROUP BY oi.menu_id ORDER BY SUM(oi.qty) DESC LIMIT 1",
            params![d0],
            |r| r.get(0),
        )
        .ok();
    let terlaris = items
        .iter()
        .find(|m| Some(m["id"].as_i64().unwrap_or(-1)) == top && m["tersedia"] == true)
        .or_else(|| items.iter().find(|m| m["tersedia"] == true))
        .cloned();
    let mut kategori: Vec<String> = Vec::new();
    for m in &items {
        let k = m["kategori"].as_str().unwrap_or("").to_string();
        if !kategori.contains(&k) {
            kategori.push(k);
        }
    }
    Ok(Json(json!({ "kategori": kategori, "items": items, "terlaris": terlaris })))
}

pub async fn meja_list(State(s): State<S>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let mut stmt = conn.prepare("SELECT no FROM meja WHERE aktif = 1 ORDER BY no")?;
    let nos: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!(nos)))
}

// ---------- pesanan (pelanggan, diakses lewat token rahasia) ----------

pub fn order_id_by_token(conn: &Connection, token: &str) -> R<i64> {
    conn.query_row("SELECT id FROM orders WHERE token = ?1", params![token], |r| r.get(0))
        .map_err(|_| not_found("pesanan tidak ditemukan"))
}

pub fn order_view(conn: &Connection, id: i64) -> R<Value> {
    let est = orders::max_waktu_masak(conn, id);
    let mut v: Value = conn
        .query_row(
            "SELECT meja_no, metode, status, total, uang_diterima, kembalian,
                    created_at, expires_at, paid_at, cooking_at, ready_at, served_at
               FROM orders WHERE id = ?1",
            params![id],
            |r| {
                let status: String = r.get(2)?;
                let cooking_at: Option<i64> = r.get(9)?;
                Ok(json!({
                    "id": id, "kode": kode(id),
                    "meja": r.get::<_, String>(0)?, "metode": r.get::<_, String>(1)?,
                    "status": status, "total": r.get::<_, i64>(3)?,
                    "uang_diterima": r.get::<_, Option<i64>>(4)?, "kembalian": r.get::<_, Option<i64>>(5)?,
                    "created_at": r.get::<_, i64>(6)?, "expires_at": r.get::<_, i64>(7)?,
                    "paid_at": r.get::<_, Option<i64>>(8)?, "cooking_at": cooking_at,
                    "ready_at": r.get::<_, Option<i64>>(10)?, "served_at": r.get::<_, Option<i64>>(11)?,
                    "estimasi_menit": orders::estimasi_menit(&status, est, cooking_at),
                }))
            },
        )
        .map_err(|_| not_found("pesanan tidak ditemukan"))?;
    v["items"] = json!(orders::items_json(conn, id)?);
    let mut stmt = conn.prepare(
        "SELECT id, tipe, judul, isi, created_at FROM notifications
          WHERE order_id = ?1 AND audience = 'pembeli' ORDER BY id",
    )?;
    let rows = stmt.query_map(params![id], |r| {
        Ok(json!({ "id": r.get::<_, i64>(0)?, "tipe": r.get::<_, String>(1)?, "judul": r.get::<_, String>(2)?,
                   "isi": r.get::<_, String>(3)?, "at": r.get::<_, i64>(4)? }))
    })?;
    v["notifikasi"] = json!(rows.collect::<Result<Vec<_>, _>>()?);
    Ok(v)
}

pub async fn create_order(State(s): State<S>, Json(b): Json<NewOrder>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let (id, token) = orders::create_order(&conn, &b)?;
    let v = order_view(&conn, id)?;
    Ok(Json(json!({ "token": token, "order": v })))
}

pub async fn get_order(State(s): State<S>, Path(token): Path<String>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let id = order_id_by_token(&conn, &token)?;
    Ok(Json(order_view(&conn, id)?))
}

pub async fn order_qr(State(s): State<S>, Path(token): Path<String>) -> R<Response> {
    let (id, total, nmid) = {
        let conn = s.db.lock().unwrap();
        let id = order_id_by_token(&conn, &token)?;
        let total: i64 = conn.query_row("SELECT total FROM orders WHERE id = ?1", params![id], |r| r.get(0))?;
        (id, total, setting_s(&conn, "merchant_nmid", "SIMULASI"))
    };
    // Payload simulasi. Untuk QRIS sungguhan, ganti dengan QR dinamis dari payment gateway.
    qr_svg(&format!("DAPURKU-SIM|{}|{}|{}", kode(id), total, nmid))
}

pub async fn meja_qr(State(s): State<S>, headers: HeaderMap, Path(no): Path<String>) -> R<Response> {
    {
        let conn = s.db.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM meja WHERE no = ?1", params![no], |r| r.get(0))?;
        if n == 0 {
            return Err(not_found("meja tidak ditemukan"));
        }
    }
    let base = base_publik(&s.cfg, &headers);
    qr_svg(&format!("{}/?meja={}", base, no))
}

/// Alamat publik aplikasi untuk ditanam di QR meja. BASE_URL dipakai kalau ada.
/// Kalau tidak, alamat disusun dari header Host; skemanya diambil dari
/// X-Forwarded-Proto bila proxy dipercaya, karena memaksa http:// membuat QR
/// yang dicetak mengarah ke alamat tidak terenkripsi di pemasangan HTTPS.
fn base_publik(cfg: &crate::Config, headers: &HeaderMap) -> String {
    if !cfg.base_url.is_empty() {
        return cfg.base_url.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");
    let skema = if cfg.trust_proxy {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim())
            .filter(|v| *v == "https" || *v == "http")
            .unwrap_or("http")
    } else {
        "http"
    };
    format!("{skema}://{host}")
}

fn qr_svg(data: &str) -> R<Response> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|_| bad("data QR terlalu panjang"))?;
    let img = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(false)
        .dark_color(svg::Color("#111311"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(([(header::CONTENT_TYPE, "image/svg+xml")], img).into_response())
}

pub async fn simulate_pay(State(s): State<S>, Path(token): Path<String>) -> R<Json<Value>> {
    if !s.cfg.sim {
        return Err(forbidden());
    }
    let conn = s.db.lock().unwrap();
    let id = order_id_by_token(&conn, &token)?;
    let total: i64 = conn.query_row("SELECT total FROM orders WHERE id = ?1", params![id], |r| r.get(0))?;
    let baru = orders::mark_paid(&conn, id, "qris", total, &format!("SIM-{id}"), None, None, None)?;
    Ok(Json(json!({ "ok": true, "baru": baru })))
}

pub async fn customer_cancel(State(s): State<S>, Path(token): Path<String>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let id = order_id_by_token(&conn, &token)?;
    orders::cancel(&conn, id, false, None)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn panggil(State(s): State<S>, Path(token): Path<String>) -> R<Json<Value>> {
    let conn = s.db.lock().unwrap();
    let id = order_id_by_token(&conn, &token)?;
    let (meja, status): (String, String) = conn.query_row(
        "SELECT meja_no, status FROM orders WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if status == "batal" || status == "kedaluwarsa" {
        return Err(conflict("pesanan sudah tidak aktif"));
    }
    // dedupe per menit supaya tidak bisa di-spam
    let menit = now() / 60;
    push(&conn, Some(id), "kasir", "panggil_pelayan", &format!("Meja {meja} memanggil pelayan"),
         &format!("#{}", kode(id)), Some(&format!("{id}:panggil:{menit}:kasir")))?;
    push(&conn, Some(id), "pembeli", "pelayan_dipanggil", "Pelayan dipanggil",
         "Pelayan segera menuju mejamu.", Some(&format!("{id}:panggil:{menit}:pembeli")))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn order_events(State(s): State<S>, headers: HeaderMap, Path(token): Path<String>) -> R<Sse<impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>> {
    let (id, last) = {
        let conn = s.db.lock().unwrap();
        let id = order_id_by_token(&conn, &token)?;
        (id, sse::start_id(&conn, &headers))
    };
    Ok(sse::stream(s.clone(), Some(id), vec!["pembeli".to_string()], last))
}

// ---------- webhook payment gateway (QRIS) ----------

#[derive(Deserialize)]
struct Hook {
    kode: String,
    #[serde(rename = "ref")]
    ref_: String,
    amount: i64,
    status: String,
}

/// Payment gateway memanggil endpoint ini. Body ditandatangani HMAC-SHA256
/// dengan WEBHOOK_SECRET dan dikirim di header `X-Signature` (hex).
pub async fn webhook_qris(State(s): State<S>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    let sig = headers.get("x-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    let want = hex::decode(sig).map_err(|_| unauthorized())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(s.cfg.webhook_secret.as_bytes()).map_err(|_| bad("secret"))?;
    mac.update(&body);
    mac.verify_slice(&want).map_err(|_| unauthorized())?;

    let h: Hook = serde_json::from_slice(&body).map_err(|_| bad("payload tidak valid"))?;
    if h.status != "PAID" {
        return Ok(Json(json!({ "ok": true, "diabaikan": true })));
    }
    let kode_bersih = h.kode.trim();
    let id: i64 = kode_bersih
        .strip_prefix("DPK-")
        .unwrap_or(kode_bersih)
        .parse()
        .map_err(|_| bad("kode tidak valid"))?;
    let conn = s.db.lock().unwrap();
    let (metode, total): (String, i64) = conn
        .query_row("SELECT metode, total FROM orders WHERE id = ?1", params![id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(|_| not_found("pesanan tidak ditemukan"))?;
    if metode != "qris" {
        return Err(conflict("pesanan ini bukan pesanan QRIS"));
    }
    // Fix 3b: nominal harus sama persis. Kalau lebih besar, kelebihannya tidak
    // tercatat di payments.jumlah (yang menyimpan total), jadi tolak daripada hilang.
    if h.amount != total {
        return Err(bad("jumlah bayar tidak sama dengan total pesanan"));
    }
    let baru = orders::mark_paid(&conn, id, "qris", h.amount, &format!("QRIS-{}", h.ref_), None, None, None)?;
    Ok(Json(json!({ "ok": true, "baru": baru })))
}

// ---------- login staf ----------

#[derive(Deserialize)]
pub struct Login {
    username: String,
    password: String,
}

/// Hash pembanding supaya username yang tidak ada tetap memakan waktu hitung
/// yang mirip; tanpa ini, lama jawaban membocorkan username mana yang terdaftar.
fn hash_umpan() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| crate::auth::hash_password("umpan"))
}

pub async fn login(
    State(s): State<S>,
    ConnectInfo(alamat): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(b): Json<Login>,
) -> R<Response> {
    let uname = b.username.trim().to_lowercase();

    // Batasi percobaan yang gagal. Per username supaya satu akun tidak bisa
    // ditebak terus-menerus, dan per IP supaya satu penebak tidak bisa
    // menyapu banyak username sekaligus.
    let ip = crate::auth::ip_klien(s.cfg.trust_proxy, &headers, alamat);
    let kunci_u = format!("u:{uname}");
    let kunci_ip = format!("ip:{ip}");
    for (kunci, batas) in [
        (&kunci_u, crate::auth::BATAS_USER),
        (&kunci_ip, crate::auth::BATAS_IP),
    ] {
        if let Some(sisa) = crate::auth::sisa_kunci(&s.login_gagal, kunci, batas) {
            return Err(too_many(&format!(
                "terlalu banyak percobaan login, coba lagi dalam {} menit",
                (sisa + 59) / 60
            )));
        }
    }

    let akun: Option<(i64, String, String, String)> = {
        let conn = s.db.lock().unwrap();
        conn.query_row(
            "SELECT id, pass_hash, role, nama FROM users WHERE username = ?1 AND aktif = 1",
            params![uname],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
    };
    let salah = || AppError(axum::http::StatusCode::UNAUTHORIZED, "username atau password salah".into());
    let (uid, hash, role, nama) = match akun {
        Some(v) => v,
        None => {
            crate::auth::verify_password(hash_umpan(), &b.password);
            crate::auth::catat_gagal(&s.login_gagal, &kunci_u);
            crate::auth::catat_gagal(&s.login_gagal, &kunci_ip);
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            return Err(salah());
        }
    };
    if !crate::auth::verify_password(&hash, &b.password) {
        crate::auth::catat_gagal(&s.login_gagal, &kunci_u);
        crate::auth::catat_gagal(&s.login_gagal, &kunci_ip);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        return Err(salah());
    }
    crate::auth::hapus_gagal(&s.login_gagal, &kunci_u);
    crate::auth::hapus_gagal(&s.login_gagal, &kunci_ip);
    let token = format!("{}{}", rand_token(24), rand_token(24));
    let n = now();
    {
        let conn = s.db.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE expires_at < ?1", params![n])?;
        conn.execute(
            "INSERT INTO sessions(token, role, nama, created_at, expires_at, user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![token, role, nama, n, n + 12 * 3600, uid],
        )?;
        conn.execute("UPDATE users SET last_login = ?2 WHERE id = ?1", params![uid, n])?;
    }
    let ck = crate::auth::cookie_sesi(&token, s.cfg.cookie_secure);
    Ok((
        [(header::SET_COOKIE, ck)],
        Json(json!({ "ok": true, "role": role, "nama": nama, "username": uname })),
    )
        .into_response())
}

pub async fn logout(State(s): State<S>, headers: HeaderMap) -> R<Response> {
    if let Some(tok) = cookie(&headers, "dk_session") {
        let conn = s.db.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE token = ?1", params![tok])?;
    }
    let ck = crate::auth::cookie_hapus(s.cfg.cookie_secure);
    Ok(([(header::SET_COOKIE, ck)], Json(json!({ "ok": true }))).into_response())
}

pub async fn me(auth: Auth) -> Json<Value> {
    Json(json!({ "role": auth.role, "nama": auth.nama, "username": auth.username }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base_url: &str, trust_proxy: bool) -> crate::Config {
        crate::Config {
            web_dir: "../web".into(),
            sim: true,
            webhook_secret: "x".into(),
            tz: 420,
            base_url: base_url.into(),
            trust_proxy,
            cookie_secure: false,
        }
    }

    fn headers(pasangan: &[(&str, &str)]) -> HeaderMap {
        use axum::http::HeaderName;
        let mut h = HeaderMap::new();
        for (k, v) in pasangan {
            h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn base_url_dipakai_kalau_diset() {
        let h = headers(&[("host", "localhost:8080")]);
        assert_eq!(
            base_publik(&cfg("https://dapurku.example.id", false), &h),
            "https://dapurku.example.id"
        );
    }

    #[test]
    fn garis_miring_di_ujung_base_url_dibuang() {
        let h = headers(&[]);
        assert_eq!(
            base_publik(&cfg("https://dapurku.example.id/", false), &h),
            "https://dapurku.example.id"
        );
    }

    #[test]
    fn tanpa_base_url_memakai_host_dan_http() {
        let h = headers(&[("host", "192.168.1.10:8080")]);
        assert_eq!(base_publik(&cfg("", false), &h), "http://192.168.1.10:8080");
    }

    #[test]
    fn skema_https_diambil_dari_proxy_yang_dipercaya() {
        let h = headers(&[("host", "dapurku.example.id"), ("x-forwarded-proto", "https")]);
        assert_eq!(base_publik(&cfg("", true), &h), "https://dapurku.example.id");
        // rantai proxy: ambil yang pertama
        let h2 = headers(&[("host", "dapurku.example.id"), ("x-forwarded-proto", "https, http")]);
        assert_eq!(base_publik(&cfg("", true), &h2), "https://dapurku.example.id");
    }

    #[test]
    fn header_proxy_diabaikan_kalau_proxy_tidak_dipercaya() {
        let h = headers(&[("host", "dapurku.example.id"), ("x-forwarded-proto", "https")]);
        assert_eq!(base_publik(&cfg("", false), &h), "http://dapurku.example.id");
    }

    #[test]
    fn skema_aneh_dari_proxy_tidak_dipakai() {
        let h = headers(&[("host", "dapurku.example.id"), ("x-forwarded-proto", "javascript")]);
        assert_eq!(base_publik(&cfg("", true), &h), "http://dapurku.example.id");
    }
}
