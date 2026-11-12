mod api_public;
mod api_staff;
mod auth;
mod db;
mod notify;
mod orders;
mod sse;
mod util;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Router,
};
use rusqlite::Connection;
use util::alasan_tolak_live;
use std::{net::SocketAddr, sync::{Arc, Mutex}};
use tower_http::services::ServeDir;

pub struct Config {
    pub web_dir: String,
    pub sim: bool,
    pub webhook_secret: String,
    pub tz: i64, // menit di atas UTC, WIB = 420
    pub base_url: String,
    pub trust_proxy: bool,
    pub cookie_secure: bool,
}

pub struct App {
    pub db: Mutex<Connection>,
    pub cfg: Config,
    pub login_gagal: auth::LoginGagal,
}

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Sajikan halaman tanpa menulis akhiran .html: /login -> web/login.html.
/// Akhiran .html tetap dilayani supaya tautan lama tidak mati.
async fn halaman(State(s): State<Arc<App>>, Path(nama): Path<String>) -> Response {
    let nama = nama.strip_suffix(".html").unwrap_or(&nama);
    let aman = !nama.is_empty()
        && nama.len() <= 48
        && nama
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if aman {
        let berkas = format!("{}/{}.html", s.cfg.web_dir, nama);
        if let Ok(isi) = tokio::fs::read_to_string(&berkas).await {
            return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], isi).into_response();
        }
    }
    (StatusCode::NOT_FOUND, "halaman tidak ditemukan").into_response()
}

#[tokio::main]
async fn main() {
    let db_path = env("DATABASE_PATH", "../data/dapurku.db");
    let port: u16 = env("PORT", "8080").parse().unwrap_or(8080);
    let base_url = env("BASE_URL", "").trim_end_matches('/').to_string();
    let cfg = Config {
        web_dir: env("WEB_DIR", "../web"),
        sim: env("PAYMENT_MODE", "sim") == "sim",
        webhook_secret: env("WEBHOOK_SECRET", "dev-secret-ganti-di-produksi"),
        tz: env("TZ_OFFSET_MIN", "420").parse().unwrap_or(420),
        trust_proxy: env("TRUST_PROXY", "0") == "1",
        // Cookie Secure membuat browser hanya mengirim cookie lewat HTTPS. Dinyalakan
        // sendiri kalau BASE_URL sudah https, dan bisa dipaksa lewat COOKIE_SECURE.
        cookie_secure: match std::env::var("COOKIE_SECURE") {
            Ok(v) => v == "1",
            Err(_) => base_url.starts_with("https://"),
        },
        base_url,
    };
    if !cfg.base_url.is_empty() && !cfg.base_url.starts_with("http://") && !cfg.base_url.starts_with("https://") {
        eprintln!("BASE_URL harus diawali http:// atau https:// (sekarang: {})", cfg.base_url);
        std::process::exit(1);
    }

    let conn = db::open(&db_path).unwrap_or_else(|e| panic!("gagal membuka database {db_path}: {e}"));

    if cfg.sim {
        eprintln!("Mode pembayaran: SIMULASI (tombol 'sudah bayar' aktif). Set PAYMENT_MODE=live untuk mematikan.");
        if cfg.webhook_secret.starts_with("dev-secret") {
            eprintln!("PERINGATAN: WEBHOOK_SECRET masih bawaan. Wajib diganti sebelum PAYMENT_MODE=live.");
        }
    } else {
        // Mode live memakai webhook pembayaran sungguhan. Lebih baik tidak jalan
        // daripada jalan dengan kredensial yang nilainya diketahui semua orang.
        let bawaan = db::akun_password_bawaan(&conn).unwrap_or_default();
        if let Some(alasan) = alasan_tolak_live(&cfg.webhook_secret, &bawaan) {
            eprintln!("PAYMENT_MODE=live ditolak: {alasan}.");
            eprintln!("Perbaiki dulu, lalu jalankan ulang:");
            eprintln!("  - set WEBHOOK_SECRET ke nilai acak panjang (minimal 16 karakter)");
            eprintln!("  - ganti password akun bawaan lewat Admin > Pengguna");
            eprintln!("Catatan: kode ini belum punya integrasi payment gateway sungguhan.");
            eprintln!("PAYMENT_MODE=live hanya mematikan tombol simulasi, bukan menyambungkan QRIS asli.");
            std::process::exit(1);
        }
        eprintln!("Mode pembayaran: LIVE (tombol simulasi mati). Ingat: belum ada gateway QRIS sungguhan di kode ini.");
    }
    if cfg.base_url.is_empty() {
        eprintln!("PERINGATAN: BASE_URL belum diset, jadi QR meja memakai alamat dari header Host.");
        eprintln!("            Set BASE_URL=https://domainmu supaya QR yang dicetak tidak salah alamat.");
    }
    let web_dir = cfg.web_dir.clone();
    let state = Arc::new(App {
        db: Mutex::new(conn),
        cfg,
        login_gagal: Default::default(),
    });

    use api_public as p;
    use api_staff as s;
    let app = Router::new()
        // publik
        .route("/api/config", get(p::config))
        .route("/api/menu", get(p::menu))
        .route("/api/meja", get(p::meja_list))
        .route("/api/meja/:no/qr.svg", get(p::meja_qr))
        .route("/api/orders", post(p::create_order))
        .route("/api/orders/:token", get(p::get_order))
        .route("/api/orders/:token/qr.svg", get(p::order_qr))
        .route("/api/orders/:token/events", get(p::order_events))
        .route("/api/orders/:token/simulate-pay", post(p::simulate_pay))
        .route("/api/orders/:token/panggil", post(p::panggil))
        .route("/api/orders/:token/batal", post(p::customer_cancel))
        .route("/api/webhook/qris", post(p::webhook_qris))
        // login
        .route("/api/login", post(p::login))
        .route("/api/logout", post(p::logout))
        .route("/api/me", get(p::me))
        // staf
        .route("/api/staff/events", get(s::staff_events))
        .route("/api/staff/dapur", get(s::dapur))
        .route("/api/staff/kasir", get(s::kasir))
        .route("/api/staff/orders", get(s::orders_list))
        .route("/api/staff/orders/:id", get(s::order_detail))
        .route("/api/staff/orders/:id/status", post(s::set_status))
        .route("/api/staff/orders/:id/bayar-tunai", post(s::bayar_tunai))
        .route("/api/staff/orders/:id/konfirmasi-qris", post(s::konfirmasi_qris))
        .route("/api/staff/orders/:id/batal", post(s::batal))
        .route("/api/staff/meja/:no/kosongkan", post(s::kosongkan))
        // admin
        .route("/api/admin/ringkasan", get(s::ringkasan))
        .route("/api/admin/orders", get(s::admin_orders))
        .route("/api/admin/menu", get(s::admin_menu).post(s::menu_create))
        .route("/api/admin/menu/:id", put(s::menu_update).delete(s::menu_hapus))
        .route("/api/admin/menu/:id/tersedia", post(s::menu_tersedia))
        .route("/api/admin/meja", get(s::admin_meja).post(s::meja_tambah))
        .route("/api/admin/meja/:no/nonaktif", post(s::meja_nonaktif))
        .route("/api/admin/laporan", get(s::laporan))
        .route("/api/admin/laporan.csv", get(s::laporan_csv))
        .route("/api/admin/settings", get(s::settings_get).put(s::settings_put))
        .route("/api/admin/users", get(s::users_list).post(s::user_create))
        .route("/api/admin/users/:id", put(s::user_update))
        // halaman tanpa akhiran .html: /login, /kasir, /dapur, ...
        .route("/:halaman", get(halaman))
        .with_state(state)
        .fallback_service(ServeDir::new(web_dir).append_index_html_on_directories(true));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    eprintln!("Dapurku berjalan di http://localhost:{port}  (database: {db_path})");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("port sudah dipakai?");
    // with_connect_info supaya handler login tahu alamat klien untuk pembatasan percobaan.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("server berhenti");
}
