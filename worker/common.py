"""Fungsi bersama worker: koneksi SQLite, setting, dan penulisan notifikasi."""
import os
import sqlite3
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DB_PATH = os.environ.get("DATABASE_PATH", str(ROOT / "data" / "dapurku.db"))


def connect():
    # Server Rust memakai WAL, jadi baca/tulis bersamaan aman. busy_timeout menunggu kalau terkunci.
    conn = sqlite3.connect(DB_PATH, timeout=5, isolation_level=None)  # autocommit; transaksi manual
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA busy_timeout=5000")
    conn.execute("PRAGMA foreign_keys=ON")
    return conn


def now():
    return int(time.time())


def setting_int(conn, key, default):
    row = conn.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
    try:
        return int(row["value"]) if row else default
    except ValueError:
        return default


def kode(order_id):
    return f"DPK-{order_id:04d}"


def push(conn, order_id, audience, tipe, judul, isi, dedupe=None):
    """Tulis notifikasi. Sama seperti notify::push di server Rust (pembeli = pending, staf = skip).

    Mengembalikan jumlah baris yang benar-benar ditulis: 0 kalau dedupe_key sudah ada.
    """
    ext = "pending" if audience == "pembeli" else "skip"
    cur = conn.execute(
        """INSERT OR IGNORE INTO notifications
           (order_id, audience, tipe, judul, isi, dedupe_key, ext_status, created_at)
           VALUES (?,?,?,?,?,?,?,?)""",
        (order_id, audience, tipe, judul, isi, dedupe, ext, now()),
    )
    return cur.rowcount
