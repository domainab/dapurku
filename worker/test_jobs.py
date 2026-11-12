"""Test pekerjaan worker. Jalankan dari folder proyek:  python3 -m unittest discover -s worker"""
import sqlite3
import unittest
from pathlib import Path

from . import jobs
from .common import now, push

ROOT = Path(__file__).resolve().parent.parent
SKEMA = (ROOT / "schema" / "schema.sql").read_text(encoding="utf-8")


class Perekam:
    """Kanal palsu yang mencatat apa saja yang dikirim."""

    name = "perekam"

    def __init__(self, gagal=False):
        self.kiriman = []
        self.gagal = gagal

    def send(self, n):
        if self.gagal:
            raise RuntimeError("kanal sedang mati")
        self.kiriman.append(n)


class DasarTest(unittest.TestCase):
    def setUp(self):
        self.conn = sqlite3.connect(":memory:", isolation_level=None)
        self.conn.row_factory = sqlite3.Row
        self.conn.executescript(SKEMA)
        self.conn.execute("INSERT INTO meja(no) VALUES ('01')")

    def tearDown(self):
        self.conn.close()

    def pesanan(self, status="menunggu_bayar", metode="qris", expires_in=900, paid_at=None, total=18000):
        t = now()
        cur = self.conn.execute(
            """INSERT INTO orders(token, meja_no, metode, status, total, created_at, expires_at, paid_at)
               VALUES (?,?,?,?,?,?,?,?)""",
            (f"tok{t}{id(self)}{self.conn.total_changes}", "01", metode, status, total,
             t, t + expires_in, paid_at),
        )
        return cur.lastrowid

    def jumlah(self, sql, *args):
        return self.conn.execute(sql, args).fetchone()[0]


class TestPush(DasarTest):
    def test_push_mengembalikan_jumlah_baris_yang_ditulis(self):
        oid = self.pesanan()
        self.assertEqual(push(self.conn, oid, "pembeli", "tes", "Judul", "Isi", f"{oid}:tes"), 1)
        # dedupe_key sama: tidak ditulis lagi, dan harus menjawab 0
        self.assertEqual(push(self.conn, oid, "pembeli", "tes", "Judul", "Isi", f"{oid}:tes"), 0)
        self.assertEqual(self.jumlah("SELECT COUNT(*) FROM notifications WHERE order_id=?", oid), 1)

    def test_tanpa_dedupe_key_selalu_ditulis(self):
        oid = self.pesanan()
        self.assertEqual(push(self.conn, oid, "pembeli", "tes", "J", "I", None), 1)
        self.assertEqual(push(self.conn, oid, "pembeli", "tes", "J", "I", None), 1)

    def test_notifikasi_pembeli_pending_staf_skip(self):
        oid = self.pesanan()
        push(self.conn, oid, "pembeli", "tes", "J", "I", f"{oid}:a")
        push(self.conn, oid, "dapur", "tes", "J", "I", f"{oid}:b")
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE audience='pembeli'"), "pending")
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE audience='dapur'"), "skip")


class TestExpireUnpaid(DasarTest):
    def test_pesanan_lewat_tenggat_jadi_kedaluwarsa(self):
        oid = self.pesanan(expires_in=-60)
        self.assertEqual(jobs.expire_unpaid(self.conn), 1)
        self.assertEqual(self.jumlah("SELECT status FROM orders WHERE id=?", oid), "kedaluwarsa")
        self.assertEqual(
            self.jumlah("SELECT COUNT(*) FROM notifications WHERE order_id=? AND tipe='pembayaran_kedaluwarsa'", oid), 1
        )

    def test_dijalankan_dua_kali_tidak_menghitung_ulang(self):
        self.pesanan(expires_in=-60)
        self.assertEqual(jobs.expire_unpaid(self.conn), 1)
        self.assertEqual(jobs.expire_unpaid(self.conn), 0)

    def test_pesanan_yang_belum_lewat_tidak_disentuh(self):
        oid = self.pesanan(expires_in=600)
        self.assertEqual(jobs.expire_unpaid(self.conn), 0)
        self.assertEqual(self.jumlah("SELECT status FROM orders WHERE id=?", oid), "menunggu_bayar")

    def test_pesanan_lunas_tidak_ikut_kedaluwarsa(self):
        oid = self.pesanan(status="baru", expires_in=-600, paid_at=now())
        self.assertEqual(jobs.expire_unpaid(self.conn), 0)
        self.assertEqual(self.jumlah("SELECT status FROM orders WHERE id=?", oid), "baru")


class TestRemindPayment(DasarTest):
    def test_menghitung_yang_benar_benar_ditulis(self):
        self.pesanan(expires_in=120)  # sisa 2 menit, masuk jendela pengingat
        self.assertEqual(jobs.remind_payment(self.conn), 1)
        # panggilan kedua ter-dedupe: harus 0, bukan jumlah baris hasil query
        self.assertEqual(jobs.remind_payment(self.conn), 0)
        self.assertEqual(self.jumlah("SELECT COUNT(*) FROM notifications WHERE tipe='pengingat_bayar'"), 1)

    def test_tunai_tidak_diingatkan(self):
        self.pesanan(metode="tunai", expires_in=120)
        self.assertEqual(jobs.remind_payment(self.conn), 0)

    def test_masih_lama_belum_diingatkan(self):
        self.pesanan(expires_in=900)
        self.assertEqual(jobs.remind_payment(self.conn), 0)


class TestLateAlerts(DasarTest):
    def test_menghitung_yang_benar_benar_ditulis(self):
        self.pesanan(status="baru", paid_at=now() - 20 * 60)
        self.assertEqual(jobs.late_alerts(self.conn), 1)
        self.assertEqual(jobs.late_alerts(self.conn), 0)
        self.assertEqual(self.jumlah("SELECT COUNT(*) FROM notifications WHERE tipe='telat'"), 1)
        self.assertEqual(self.jumlah("SELECT COUNT(*) FROM notifications WHERE tipe='pesanan_terlambat'"), 1)

    def test_belum_telat_tidak_diperingatkan(self):
        self.pesanan(status="baru", paid_at=now() - 60)
        self.assertEqual(jobs.late_alerts(self.conn), 0)


class TestDispatchOutbox(DasarTest):
    def test_meneruskan_notifikasi_pembeli(self):
        oid = self.pesanan()
        push(self.conn, oid, "pembeli", "tes", "Judul", "Isi", f"{oid}:a")
        ch = Perekam()
        self.assertEqual(jobs.dispatch_outbox(self.conn, [ch]), 1)
        self.assertEqual(len(ch.kiriman), 1)
        self.assertEqual(ch.kiriman[0]["kode"], f"DPK-{oid:04d}")
        self.assertEqual(ch.kiriman[0]["meja"], "01")
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE order_id=?", oid), "sent")

    def test_notifikasi_tanpa_order_id_tetap_terkirim(self):
        # tanpa order_id, JOIN biasa akan membuang baris ini sehingga macet 'pending' selamanya
        push(self.conn, None, "pembeli", "pengumuman", "Tutup lebih awal", "Hari ini tutup 20.00", "global:1")
        ch = Perekam()
        self.assertEqual(jobs.dispatch_outbox(self.conn, [ch]), 1)
        self.assertEqual(ch.kiriman[0]["kode"], "-")
        self.assertEqual(ch.kiriman[0]["meja"], "-")
        self.assertEqual(self.jumlah("SELECT COUNT(*) FROM notifications WHERE ext_status='pending'"), 0)

    def test_notifikasi_staf_tidak_ikut_diteruskan(self):
        oid = self.pesanan()
        push(self.conn, oid, "dapur", "tes", "J", "I", f"{oid}:d")
        ch = Perekam()
        self.assertEqual(jobs.dispatch_outbox(self.conn, [ch]), 0)
        self.assertEqual(ch.kiriman, [])

    def test_kanal_gagal_ditandai_failed_bukan_meledak(self):
        oid = self.pesanan()
        push(self.conn, oid, "pembeli", "tes", "J", "I", f"{oid}:a")
        self.assertEqual(jobs.dispatch_outbox(self.conn, [Perekam(gagal=True)]), 0)
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE order_id=?", oid), "failed")

    def test_satu_kanal_gagal_tidak_menghentikan_yang_lain(self):
        oid = self.pesanan()
        push(self.conn, oid, "pembeli", "tes", "J", "I", f"{oid}:a")
        baik = Perekam()
        self.assertEqual(jobs.dispatch_outbox(self.conn, [Perekam(gagal=True), baik]), 1)
        self.assertEqual(len(baik.kiriman), 1)
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE order_id=?", oid), "sent")

    def test_notifikasi_lama_dilewati(self):
        oid = self.pesanan()
        self.conn.execute(
            """INSERT INTO notifications(order_id, audience, tipe, judul, isi, ext_status, created_at)
               VALUES (?, 'pembeli', 'tes', 'J', 'I', 'pending', ?)""",
            (oid, now() - 7200),
        )
        ch = Perekam()
        self.assertEqual(jobs.dispatch_outbox(self.conn, [ch]), 0)
        self.assertEqual(ch.kiriman, [])
        self.assertEqual(self.jumlah("SELECT ext_status FROM notifications WHERE order_id=?", oid), "skip")


if __name__ == "__main__":
    unittest.main()
