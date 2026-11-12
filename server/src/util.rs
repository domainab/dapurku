use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::Connection;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct AppError(pub StatusCode, pub String);
pub type R<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        eprintln!("[db] {e}");
        AppError(StatusCode::INTERNAL_SERVER_ERROR, "kesalahan database".into())
    }
}

pub fn bad(msg: &str) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.to_string())
}
pub fn not_found(msg: &str) -> AppError {
    AppError(StatusCode::NOT_FOUND, msg.to_string())
}
pub fn conflict(msg: &str) -> AppError {
    AppError(StatusCode::CONFLICT, msg.to_string())
}
pub fn unauthorized() -> AppError {
    AppError(StatusCode::UNAUTHORIZED, "belum login".to_string())
}
pub fn too_many(msg: &str) -> AppError {
    AppError(StatusCode::TOO_MANY_REQUESTS, msg.to_string())
}
pub fn forbidden() -> AppError {
    AppError(StatusCode::FORBIDDEN, "tidak punya akses".to_string())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 143 -> "DPK-0143"
pub fn kode(id: i64) -> String {
    format!("DPK-{:04}", id)
}

/// 36000 -> "Rp36.000"
pub fn rupiah(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    format!("{}Rp{}", if n < 0 { "-" } else { "" }, out)
}

/// Alasan kenapa PAYMENT_MODE=live belum boleh dijalankan, atau None kalau aman.
/// Mode live memakai webhook pembayaran sungguhan, jadi kredensial bawaan di sana
/// berarti siapa pun yang tahu nilai bawaannya bisa menandai pesanan lunas.
pub fn alasan_tolak_live(webhook_secret: &str, akun_bawaan: &[String]) -> Option<String> {
    let mut sebab: Vec<String> = Vec::new();
    if webhook_secret.starts_with("dev-secret") {
        sebab.push("WEBHOOK_SECRET masih memakai nilai bawaan".to_string());
    } else if webhook_secret.chars().count() < 16 {
        sebab.push("WEBHOOK_SECRET terlalu pendek (minimal 16 karakter)".to_string());
    }
    if !akun_bawaan.is_empty() {
        sebab.push(format!(
            "akun ini masih memakai password bawaan: {}",
            akun_bawaan.join(", ")
        ));
    }
    if sebab.is_empty() {
        None
    } else {
        Some(sebab.join("; "))
    }
}

pub fn setting_s(conn: &Connection, key: &str, def: &str) -> String {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .unwrap_or_else(|_| def.to_string())
}

pub fn setting_i(conn: &Connection, key: &str, def: i64) -> i64 {
    setting_s(conn, key, "").parse::<i64>().unwrap_or(def)
}

/// Awal hari (epoch detik, UTC) menurut zona waktu restoran.
pub fn day_start(t: i64, tz_off_min: i64) -> i64 {
    let off = tz_off_min * 60;
    (t + off).div_euclid(86400) * 86400 - off
}

pub fn rand_token(n: usize) -> String {
    use rand::Rng;
    const A: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| A[rng.gen_range(0..A.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rupiah() {
        assert_eq!(rupiah(0), "Rp0");
        assert_eq!(rupiah(999), "Rp999");
        assert_eq!(rupiah(1000), "Rp1.000");
        assert_eq!(rupiah(36000), "Rp36.000");
        assert_eq!(rupiah(1234567), "Rp1.234.567");
        assert_eq!(rupiah(-2000), "-Rp2.000");
    }

    #[test]
    fn format_kode_pesanan() {
        assert_eq!(kode(1), "DPK-0001");
        assert_eq!(kode(143), "DPK-0143");
        assert_eq!(kode(12345), "DPK-12345");
    }

    #[test]
    fn awal_hari_mengikuti_zona_restoran() {
        // WIB = UTC+7. 2024-01-01 00:30 WIB = 2023-12-31 17:30 UTC.
        let wib = 420;
        let t = 1704044400; // 2023-12-31T17:00:00Z
        let d0 = day_start(t, wib);
        assert_eq!(d0, 1704042000, "awal hari WIB = 17:00 UTC hari sebelumnya");
        assert!(d0 <= t && t < d0 + 86400);
        // satu detik sebelum tengah malam WIB masih hari yang sama
        assert_eq!(day_start(d0 + 86399, wib), d0);
        assert_eq!(day_start(d0 + 86400, wib), d0 + 86400);
    }

    #[test]
    fn live_ditolak_kalau_webhook_secret_masih_bawaan() {
        let a = alasan_tolak_live("dev-secret-ganti-di-produksi", &[]).unwrap();
        assert!(a.contains("WEBHOOK_SECRET"), "{a}");
    }

    #[test]
    fn live_ditolak_kalau_webhook_secret_terlalu_pendek() {
        assert!(alasan_tolak_live("pendek", &[]).is_some());
        assert!(alasan_tolak_live("123456789012345", &[]).is_some(), "15 karakter masih kurang");
        assert!(alasan_tolak_live("1234567890123456", &[]).is_none(), "16 karakter sudah cukup");
    }

    #[test]
    fn live_ditolak_kalau_masih_ada_password_bawaan() {
        let secret = "rahasia-panjang-sekali";
        let a = alasan_tolak_live(secret, &["admin".into(), "kasir".into()]).unwrap();
        assert!(a.contains("admin") && a.contains("kasir"), "{a}");
    }

    #[test]
    fn live_diizinkan_kalau_semuanya_sudah_diganti() {
        assert_eq!(alasan_tolak_live("rahasia-panjang-sekali", &[]), None);
    }

    #[test]
    fn dua_alasan_dilaporkan_sekaligus() {
        let a = alasan_tolak_live("dev-secret-x", &["admin".into()]).unwrap();
        assert!(a.contains("WEBHOOK_SECRET") && a.contains("admin"), "{a}");
    }

    #[test]
    fn token_acak_panjangnya_pas_dan_berbeda() {
        let a = rand_token(16);
        let b = rand_token(16);
        assert_eq!(a.chars().count(), 16);
        assert_ne!(a, b);
    }
}
