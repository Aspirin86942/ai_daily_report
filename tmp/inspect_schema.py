import sqlite3

con = sqlite3.connect('data/db/scan_index_v3.sqlite3')
con.row_factory = sqlite3.Row
# Inspect schema of key tables
for t in ['file_inventory', 'parse_cache', 'context_runs', 'context_decisions']:
    print('==== schema:', t)
    for row in con.execute(f"PRAGMA table_info({t})"):
        print(' ', dict(row))
con.close()
