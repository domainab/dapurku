"""Worker Dapurku. Jalankan dari folder proyek:  python -m worker.main"""
import os
import time

from . import jobs
from .common import DB_PATH, connect


def main():
    interval = float(os.environ.get("WORKER_INTERVAL", "3"))
    print(f"Worker Dapurku jalan. Database: {DB_PATH}, interval {interval}s", flush=True)
    conn = None
    while True:
        try:
            if conn is None:
                conn = connect()
            e = jobs.expire_unpaid(conn)
            r = jobs.remind_payment(conn)
            l = jobs.late_alerts(conn)
            s = jobs.dispatch_outbox(conn)
            if e or r or l or s:
                print(f"[worker] kedaluwarsa={e} pengingat={r} telat={l} notif_diteruskan={s}",
                      flush=True)
        except Exception as ex:  # DB belum ada / terkunci: coba lagi nanti
            print(f"[worker] error: {ex}", flush=True)
            conn = None
        time.sleep(interval)


if __name__ == "__main__":
    main()
