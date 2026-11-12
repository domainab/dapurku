use crate::util::*;
use crate::App;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{async_trait, extract::FromRequestParts, http::request::Parts, http::HeaderMap};
use rusqlite::params;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub struct Auth {
    pub role: String,
    pub nama: String,
    pub username: String,
    /// Token sesi yang dipakai permintaan ini, supaya sesi lain bisa diputus
    /// tanpa ikut memutus sesi yang sedang berjalan.
    pub token: String,
}

/// Hash password dengan Argon2id. Hasilnya format PHC, salt sudah ikut di dalamnya.
pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .expect("gagal menghitung hash password")
        .to_string()
}

/// Cocokkan password dengan hash tersimpan. Hash rusak = selalu gagal, bukan panik.
pub fn verify_password(hash: &str, pw: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(h) => Argon2::default().verify_password(pw.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

/// Hitungan percobaan login yang gagal: kunci -> (jumlah, waktu percobaan pertama).
/// Disimpan di memori saja; restart server menghapusnya, dan itu memang cukup
/// untuk menahan percobaan menebak yang berlangsung cepat.
pub type LoginGagal = Mutex<HashMap<String, (u32, i64)>>;

/// Batas kegagalan per username dalam satu jendela. Ini pertahanan utamanya.
pub const BATAS_USER: u32 = 10;
/// Batas kegagalan per alamat IP. Jauh lebih longgar dengan sengaja: satu
/// restoran biasanya berbagi satu IP publik, jadi ambang serendah batas
/// username akan membuat satu orang yang salah ketik mengunci seluruh staf.
/// Angka ini tetap membuat penebakan kamus tidak ada gunanya (50 per 15 menit).
pub const BATAS_IP: u32 = 50;
pub const JENDELA_DETIK: i64 = 900; // 15 menit

fn sisa_kunci_pada(peta: &LoginGagal, kunci: &str, batas: u32, t: i64) -> Option<i64> {
    let m = peta.lock().unwrap();
    match m.get(kunci) {
        Some((n, mulai)) if *n >= batas && t - mulai < JENDELA_DETIK => {
            Some(JENDELA_DETIK - (t - mulai))
        }
        _ => None,
    }
}

/// Sisa detik penguncian, atau None kalau kunci ini masih boleh mencoba.
pub fn sisa_kunci(peta: &LoginGagal, kunci: &str, batas: u32) -> Option<i64> {
    sisa_kunci_pada(peta, kunci, batas, now())
}

fn catat_gagal_pada(peta: &LoginGagal, kunci: &str, t: i64) {
    let mut m = peta.lock().unwrap();
    // buang entri yang jendelanya sudah lewat supaya peta tidak tumbuh terus
    m.retain(|_, v| t - v.1 < JENDELA_DETIK);
    let e = m.entry(kunci.to_string()).or_insert((0, t));
    if t - e.1 >= JENDELA_DETIK {
        *e = (0, t);
    }
    e.0 += 1;
}

pub fn catat_gagal(peta: &LoginGagal, kunci: &str) {
    catat_gagal_pada(peta, kunci, now())
}

pub fn hapus_gagal(peta: &LoginGagal, kunci: &str) {
    peta.lock().unwrap().remove(kunci);
}

/// Alamat klien untuk keperluan pembatasan. Di belakang reverse proxy, alamat
/// socket selalu alamat proxy itu sendiri, sehingga semua pengguna akan berbagi
/// satu hitungan; karena itu X-Forwarded-For dipakai kalau TRUST_PROXY=1.
/// Header itu bisa dipalsukan, jadi pembatasan per username yang jadi
/// pertahanan utama dan pembatasan per IP hanya pelengkap.
pub fn ip_klien(trust_proxy: bool, headers: &HeaderMap, socket: SocketAddr) -> String {
    if trust_proxy {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(ip) = xff.split(',').next() {
                let ip = ip.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    socket.ip().to_string()
}

/// Cookie sesi. `secure` membuat browser hanya mengirimkannya lewat HTTPS;
/// di HTTP biasa (pengembangan atau jaringan lokal) flag itu justru membuat
/// cookie dibuang sehingga login tidak pernah menempel, karena itu ia ikut
/// konfigurasi, bukan dinyalakan begitu saja.
pub fn cookie_sesi(token: &str, secure: bool) -> String {
    format!(
        "dk_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200{}",
        if secure { "; Secure" } else { "" }
    )
}

/// Penghapus cookie sesi. Atributnya harus sama dengan saat dibuat, kalau tidak
/// browser menganggapnya cookie lain dan yang lama tetap tertinggal.
pub fn cookie_hapus(secure: bool) -> String {
    format!(
        "dk_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

#[async_trait]
impl FromRequestParts<Arc<App>> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<App>) -> Result<Self, Self::Rejection> {
        let tok = cookie(&parts.headers, "dk_session").ok_or_else(unauthorized)?;
        let conn = state.db.lock().unwrap();
        // Akun yang dinonaktifkan langsung kehilangan sesinya.
        conn.query_row(
            "SELECT s.role, s.nama, COALESCE(u.username, '')
               FROM sessions s LEFT JOIN users u ON u.id = s.user_id
              WHERE s.token = ?1 AND s.expires_at > ?2 AND (s.user_id IS NULL OR u.aktif = 1)",
            params![tok, now()],
            |r| {
                Ok(Auth {
                    role: r.get(0)?,
                    nama: r.get(1)?,
                    username: r.get(2)?,
                    token: tok.clone(),
                })
            },
        )
        .map_err(|_| unauthorized())
    }
}

impl Auth {
    /// Admin boleh semuanya. Role lain harus ada di daftar.
    pub fn need(&self, roles: &[&str]) -> R<()> {
        if self.role == "admin" || roles.contains(&self.role.as_str()) {
            Ok(())
        } else {
            Err(forbidden())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_cocok_dengan_hashnya() {
        let h = hash_password("rahasia123");
        assert!(verify_password(&h, "rahasia123"));
        assert!(!verify_password(&h, "rahasia124"));
        assert!(!verify_password(&h, ""));
    }

    #[test]
    fn hash_tidak_menyimpan_password_apa_adanya() {
        let h = hash_password("rahasia123");
        assert!(!h.contains("rahasia123"));
        assert!(h.starts_with("$argon2"));
    }

    #[test]
    fn dua_hash_dari_password_sama_tetap_berbeda() {
        // salt acak: dua pengguna dengan password sama tidak terlihat sama di database
        assert_ne!(hash_password("rahasia123"), hash_password("rahasia123"));
    }

    #[test]
    fn hash_rusak_ditolak_bukan_panik() {
        assert!(!verify_password("bukan-hash", "apa pun"));
        assert!(!verify_password("", "apa pun"));
    }

    #[test]
    fn cookie_sesi_tanpa_secure_di_http() {
        let c = cookie_sesi("abc123", false);
        assert!(c.starts_with("dk_session=abc123;"));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax"));
        assert!(!c.contains("Secure"), "di HTTP, Secure justru membuat cookie dibuang");
    }

    #[test]
    fn cookie_sesi_pakai_secure_di_https() {
        assert!(cookie_sesi("abc123", true).ends_with("; Secure"));
    }

    #[test]
    fn cookie_hapus_atributnya_sama_dengan_saat_dibuat() {
        for secure in [false, true] {
            let buat = cookie_sesi("abc123", secure);
            let hapus = cookie_hapus(secure);
            assert!(hapus.contains("Max-Age=0"));
            // atribut selain nilai dan Max-Age harus persis sama, kalau tidak
            // browser menganggapnya cookie berbeda dan yang lama tertinggal
            for atribut in ["Path=/", "HttpOnly", "SameSite=Lax"] {
                assert!(buat.contains(atribut) && hapus.contains(atribut), "{atribut}");
            }
            assert_eq!(buat.contains("; Secure"), hapus.contains("; Secure"));
        }
    }

    const T: i64 = 1_700_000_000;

    #[test]
    fn di_bawah_batas_masih_boleh_mencoba() {
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER - 1 {
            catat_gagal_pada(&peta, "u:admin", T);
        }
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T), None);
    }

    #[test]
    fn mencapai_batas_mengunci_sampai_jendela_lewat() {
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER {
            catat_gagal_pada(&peta, "u:admin", T);
        }
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T), Some(JENDELA_DETIK));
        // masih terkunci satu detik sebelum jendela berakhir
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T + JENDELA_DETIK - 1), Some(1));
        // tepat saat jendela berakhir sudah boleh lagi
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T + JENDELA_DETIK), None);
    }

    #[test]
    fn login_berhasil_menghapus_hitungan() {
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER {
            catat_gagal_pada(&peta, "u:admin", T);
        }
        assert!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T).is_some());
        hapus_gagal(&peta, "u:admin");
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T), None);
    }

    #[test]
    fn kunci_satu_akun_tidak_mengunci_akun_lain() {
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER {
            catat_gagal_pada(&peta, "u:admin", T);
        }
        assert!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T).is_some());
        assert_eq!(sisa_kunci_pada(&peta, "u:kasir", BATAS_USER, T), None);
    }

    #[test]
    fn satu_staf_salah_ketik_tidak_mengunci_satu_kantor() {
        // semua staf berbagi satu IP publik. Satu orang mentok di batas
        // username-nya sendiri tidak boleh ikut memenuhi batas IP.
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER {
            catat_gagal_pada(&peta, "u:admin", T);
            catat_gagal_pada(&peta, "ip:203.0.113.9", T);
        }
        assert!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T).is_some(), "akunnya sendiri terkunci");
        assert_eq!(
            sisa_kunci_pada(&peta, "ip:203.0.113.9", BATAS_IP, T),
            None,
            "rekan sekantor harus tetap bisa login"
        );
    }

    #[test]
    fn penebak_yang_menyapu_banyak_username_tetap_kena_batas_ip() {
        let peta: LoginGagal = Default::default();
        for i in 0..BATAS_IP {
            catat_gagal_pada(&peta, &format!("u:korban{i}"), T);
            catat_gagal_pada(&peta, "ip:198.51.100.7", T);
        }
        assert!(sisa_kunci_pada(&peta, "ip:198.51.100.7", BATAS_IP, T).is_some());
    }

    #[test]
    fn hitungan_mulai_dari_nol_di_jendela_baru() {
        let peta: LoginGagal = Default::default();
        for _ in 0..BATAS_USER {
            catat_gagal_pada(&peta, "u:admin", T);
        }
        // satu kegagalan di jendela berikutnya tidak boleh langsung mengunci lagi
        catat_gagal_pada(&peta, "u:admin", T + JENDELA_DETIK + 1);
        assert_eq!(sisa_kunci_pada(&peta, "u:admin", BATAS_USER, T + JENDELA_DETIK + 1), None);
    }

    #[test]
    fn entri_kedaluwarsa_dibuang_supaya_peta_tidak_membengkak() {
        let peta: LoginGagal = Default::default();
        for i in 0..50 {
            catat_gagal_pada(&peta, &format!("ip:10.0.0.{i}"), T);
        }
        assert_eq!(peta.lock().unwrap().len(), 50);
        catat_gagal_pada(&peta, "ip:10.0.1.1", T + JENDELA_DETIK + 1);
        assert_eq!(peta.lock().unwrap().len(), 1, "entri lama harus terbuang");
    }

    #[test]
    fn ip_memakai_alamat_socket_kalau_proxy_tidak_dipercaya() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.5".parse().unwrap());
        let socket: SocketAddr = "10.1.2.3:51000".parse().unwrap();
        assert_eq!(ip_klien(false, &h, socket), "10.1.2.3");
    }

    #[test]
    fn ip_memakai_x_forwarded_for_kalau_proxy_dipercaya() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.5, 10.0.0.1".parse().unwrap());
        let socket: SocketAddr = "10.1.2.3:51000".parse().unwrap();
        assert_eq!(ip_klien(true, &h, socket), "203.0.113.5");
        // tanpa header, tetap jatuh ke alamat socket
        assert_eq!(ip_klien(true, &HeaderMap::new(), socket), "10.1.2.3");
    }
}
