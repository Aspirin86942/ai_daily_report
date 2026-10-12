import sqlite3

for db in ['data/db/scan_index_v2.sqlite3', 'data/db/scan_index.sqlite3']:
    print('='*80)
    print('DB:', db)
    con = sqlite3.connect(db)
    con.row_factory = sqlite3.Row
    try:
        tables = [r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")]
        for t in tables:
            cols = [r[1] for r in con.execute(f"PRAGMA table_info({t})")]
            if 'content' in cols:
                for kw in ['财务管理中心负责人(OA)', '财经中心-部门直属负责人', '财务管理中心负责人']:
                    try:
                        rows = list(con.execute(f"SELECT file_identity FROM {t} WHERE content LIKE ? LIMIT 10", (f'%{kw}%',)))
                    except Exception as e:
                        rows = []
                    if rows:
                        print(f'  [{t}] "{kw}":')
                        for r in rows:
                            print('    ', r['file_identity'])
    except Exception as e:
        print('  err', e)
    con.close()
