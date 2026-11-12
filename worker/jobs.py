"""Pekerjaan latar belakang. Masing-masing aman dijalankan berulang (idempoten)."""
from .channels import enabled_channels
from .common import kode, now, push, setting_int


def expire_unpaid(conn):
    """Pesanan yang lewat batas bayar -> kedaluwarsa, pembeli diberi tahu."""
    t = now()
    rows = conn.execute(
        "SELECT id, meja_no FROM orders WHERE status='menunggu_bayar' AND expires_at < ?", (t,)
    ).fetchall()
    n = 0
    for r in rows:
        conn.execute("BEGIN IMMEDIATE")
        try:
            # guard status supaya tidak menimpa pesanan yang baru saja dibayar
            cur = conn.execute(
                "UPDATE orders SET status='kedaluwarsa', cancelled_at=? WHERE id=? AND status='menunggu_bayar'",
                (t, r["id"]),
            )
            if cur.rowcount:
                push(conn, r["id"], "pembeli", "pembayaran_kedaluwarsa", "Waktu bayar habis",
                     f"Pesanan #{kode(r['id'])} dibatalkan karena belum dibayar. Silakan pesan lagi.",
                     f"{r['id']}:kedaluwarsa")
                n += 1
            conn.execute("COMMIT")
        except Exception:
            conn.execute("ROLLBACK")
            raise
    return n


def remind_payment(conn, menit_sebelum=5):
    """Ingatkan pembeli QRIS saat sisa waktu bayar tinggal beberapa menit."""
    t = now()
    rows = conn.execute(
        """SELECT id, meja_no, expires_at FROM orders
           WHERE status='menunggu_bayar' AND metode='qris'
             AND expires_at - ? <= ? AND expires_at > ?""",
        (t, menit_sebelum * 60, t),
    ).fetchall()
    n = 0
    for r in rows:
        sisa = max(1, (r["expires_at"] - t + 59) // 60)
        n += push(conn, r["id"], "pembeli", "pengingat_bayar", "Segera selesaikan pembayaran",
                  f"Sisa waktu bayar pesanan #{kode(r['id'])} sekitar {sisa} menit lagi.",
                  f"{r['id']}:pengingat_bayar")
    return n


def late_alerts(conn):
    """Pesanan lunas yang terlalu lama belum siap -> peringatan ke dapur dan permintaan maaf ke pembeli."""
    batas = setting_int(conn, "telat_menit", 15) * 60
    t = now()
    rows = conn.execute(
        "SELECT id, meja_no, paid_at FROM orders WHERE status IN ('baru','dimasak') AND paid_at IS NOT NULL AND ? - paid_at >= ?",
        (t, batas),
    ).fetchall()
    n = 0
    for r in rows:
        mnt = (t - r["paid_at"]) // 60
        a = push(conn, r["id"], "dapur", "telat", f"Terlambat · Meja {r['meja_no']}",
                 f"#{kode(r['id'])} sudah {mnt} menit sejak dibayar.", f"{r['id']}:telat")
        b = push(conn, r["id"], "pembeli", "pesanan_terlambat", "Maaf, pesananmu agak lama",
                 f"Pesanan #{kode(r['id'])} masih kami kerjakan. Terima kasih sudah menunggu.",
                 f"{r['id']}:pesanan_terlambat")
        if a or b:
            n += 1
    return n


def dispatch_outbox(conn, channels=None):
    """Teruskan notifikasi pembeli yang masih 'pending' ke kanal luar."""
    channels = enabled_channels() if channels is None else channels
    t = now()
    # notifikasi lama (misal worker baru menyala setelah lama mati) tidak perlu dikirim
    conn.execute(
        "UPDATE notifications SET ext_status='skip' WHERE ext_status='pending' AND created_at < ?",
        (t - 3600,),
    )
    rows = conn.execute(
        """SELECT n.id, n.tipe, n.judul, n.isi, n.created_at, n.order_id AS oid, o.meja_no
           FROM notifications n LEFT JOIN orders o ON o.id = n.order_id
           WHERE n.audience='pembeli' AND n.ext_status='pending' ORDER BY n.id LIMIT 100"""
    ).fetchall()
    sent = 0
    for r in rows:
        if not channels:
            status = "skip"
        else:
            payload = {"kode": kode(r["oid"]) if r["oid"] is not None else "-",
                       "meja": r["meja_no"] or "-", "tipe": r["tipe"],
                       "judul": r["judul"], "isi": r["isi"], "at": r["created_at"]}
            ok = 0
            for ch in channels:
                try:
                    ch.send(payload)
                    ok += 1
                except Exception as e:  # satu kanal gagal tidak boleh menghentikan yang lain
                    print(f"[notif] kanal {ch.name} gagal: {e}", flush=True)
            status = "sent" if ok else "failed"
            sent += 1 if ok else 0
        conn.execute("UPDATE notifications SET ext_status=? WHERE id=?", (status, r["id"]))
    return sent
