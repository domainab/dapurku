-- Skema database Dapurku (SQLite). Dipakai bersama oleh server Rust dan worker Python.
-- Semua waktu disimpan sebagai epoch detik (UTC).

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS menu_items (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  nama        TEXT    NOT NULL,
  kategori    TEXT    NOT NULL,
  harga       INTEGER NOT NULL CHECK (harga >= 0),
  deskripsi   TEXT    NOT NULL DEFAULT '',
  foto_url    TEXT    NOT NULL DEFAULT '',
  pedas       INTEGER NOT NULL DEFAULT 0,     -- 1 = ada pilihan level pedas 0-5
  waktu_masak INTEGER NOT NULL DEFAULT 10,    -- menit, dipakai untuk estimasi
  tersedia    INTEGER NOT NULL DEFAULT 1,     -- 0 = habis (tampil "Habis")
  aktif       INTEGER NOT NULL DEFAULT 1,     -- 0 = dihapus (soft delete)
  urutan      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS menu_extras (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  menu_id INTEGER NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
  nama    TEXT    NOT NULL,
  harga   INTEGER NOT NULL CHECK (harga >= 0)
);

CREATE TABLE IF NOT EXISTS meja (
  no    TEXT PRIMARY KEY,
  aktif INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS orders (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  token         TEXT    NOT NULL UNIQUE,      -- rahasia di URL pelanggan
  meja_no       TEXT    NOT NULL REFERENCES meja(no),
  metode        TEXT    NOT NULL CHECK (metode IN ('qris','tunai')),
  status        TEXT    NOT NULL CHECK (status IN
                  ('menunggu_bayar','baru','dimasak','siap','selesai','batal','kedaluwarsa')),
  total         INTEGER NOT NULL,
  uang_diterima INTEGER,
  kembalian     INTEGER,
  dibersihkan   INTEGER NOT NULL DEFAULT 0,   -- 1 = kasir sudah mengosongkan meja
  kasir         TEXT,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  paid_at       INTEGER,
  cooking_at    INTEGER,
  ready_at      INTEGER,
  served_at     INTEGER,
  cancelled_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_orders_status  ON orders(status);
CREATE INDEX IF NOT EXISTS idx_orders_meja    ON orders(meja_no, status);
CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at);

CREATE TABLE IF NOT EXISTS order_items (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  order_id    INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
  menu_id     INTEGER NOT NULL REFERENCES menu_items(id),
  nama        TEXT    NOT NULL,
  harga       INTEGER NOT NULL,               -- harga satuan termasuk tambahan
  qty         INTEGER NOT NULL CHECK (qty > 0),
  level_pedas INTEGER,
  extras      TEXT    NOT NULL DEFAULT '',    -- nama tambahan dipisah koma
  catatan     TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_items_order ON order_items(order_id);

CREATE TABLE IF NOT EXISTS payments (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  order_id   INTEGER NOT NULL REFERENCES orders(id),
  metode     TEXT    NOT NULL,
  jumlah     INTEGER NOT NULL,
  ref        TEXT    NOT NULL UNIQUE,         -- idempoten: satu ref hanya dihitung sekali
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payments_created ON payments(created_at);

-- Pengembalian dana saat pesanan yang sudah lunas dibatalkan. Dicatat terpisah
-- supaya omzet hari pembayaran tidak berubah surut; laporan memakai
-- payments dikurangi refunds pada periode masing-masing.
CREATE TABLE IF NOT EXISTS refunds (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  order_id   INTEGER NOT NULL REFERENCES orders(id),
  metode     TEXT    NOT NULL,
  jumlah     INTEGER NOT NULL,
  alasan     TEXT    NOT NULL DEFAULT '',
  kasir      TEXT,
  ref        TEXT    NOT NULL UNIQUE,         -- idempoten: satu pembatalan satu baris
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refunds_created ON refunds(created_at);

-- Antrean notifikasi. Server Rust dan worker Python sama-sama menulis ke sini.
-- audience: pembeli | dapur | kasir.  ext_status dipakai worker untuk kanal luar.
CREATE TABLE IF NOT EXISTS notifications (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  order_id   INTEGER REFERENCES orders(id) ON DELETE CASCADE,
  audience   TEXT    NOT NULL CHECK (audience IN ('pembeli','dapur','kasir')),
  tipe       TEXT    NOT NULL,
  judul      TEXT    NOT NULL,
  isi        TEXT    NOT NULL,
  dedupe_key TEXT    UNIQUE,
  ext_status TEXT    NOT NULL DEFAULT 'skip', -- skip | pending | sent | failed
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_notif_order ON notifications(order_id, id);
CREATE INDEX IF NOT EXISTS idx_notif_aud   ON notifications(audience, id);

-- Akun staf. Login memakai username + password (hash Argon2id, bukan PIN bersama).
CREATE TABLE IF NOT EXISTS users (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  username   TEXT    NOT NULL UNIQUE COLLATE NOCASE,
  pass_hash  TEXT    NOT NULL,
  role       TEXT    NOT NULL CHECK (role IN ('kasir','dapur','admin')),
  nama       TEXT    NOT NULL,
  aktif      INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  last_login INTEGER,
  -- 1 = password masih bawaan hasil seeding. Mode live menolak jalan selama
  -- masih ada akun aktif bertanda ini.
  pass_bawaan INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sessions (
  token      TEXT PRIMARY KEY,
  role       TEXT NOT NULL CHECK (role IN ('kasir','dapur','admin')),
  nama       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  user_id    INTEGER REFERENCES users(id)
);
