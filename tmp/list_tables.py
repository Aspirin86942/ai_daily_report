import sqlite3

for db in ['data/db/scan_index_v3.sqlite3', 'data/db/scan_index_v2.sqlite3',
           'data/db/scan_index.sqlite3', 'data/db/reports.sqlite3']:
    print('====', db)
    con = sqlite3.connect(db)
    for (name,) in con.execute("SELECT name FROM sqlite_master WHERE type='table'"):
        print(' table:', name)
    con.close()
